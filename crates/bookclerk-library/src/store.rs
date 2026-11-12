//! SeaORM-backed, async library store.
//!
//! [`LibraryStore`] holds a [`DatabaseConnection`] opened by the database plugin
//! (SQLite / D1 / Postgres guest) and drives the majority of CRUD through the
//! typed SeaORM [`entities`](crate::entities) (`Entity::find`, `ActiveModel`,
//! `QueryFilter`). A handful of upserts with COALESCE/precedence semantics are
//! expressed as load-then-merge over the same entities so they stay
//! backend-portable. Timestamps live in the DB as RFC 3339 `TEXT`; records
//! expose `chrono::DateTime<Utc>` and conversions happen at the record boundary.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use chrono::Utc;
use sea_orm::{
    ActiveModelTrait,
    ActiveValue::{NotSet, Set},
    ColumnTrait, Condition, ConnectionTrait, DatabaseConnection, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, TransactionTrait,
};
use uuid::Uuid;

use crate::atomic_txn::AtomicTxnBackend;
use crate::control_plane::{batch, sea_probe, Bump, LiveTopic};
use crate::entities::{
    account_links, accounts, books, claim_tickets, embeddings, encrypted_secrets, ignored_titles,
    listening_progress, oidc_auth_codes, oidc_clients, oidc_refresh_tokens, oidc_rp_states,
    operator_sessions, portal_identities, portal_sessions, saved_filters, security_audit_events,
    title_request_sources, title_requests, user_invites, user_preferences, users,
    webauthn_challenges, webauthn_credentials,
};
use crate::error::{LibraryError, Result};
use crate::models::{
    user_prefs_key, AccountRecord, AcquireStatus, AcquireStatusCounts, BookRecord,
    GlobalQueueEntry, ListeningProgressRecord, OidcClientRecord, PortalIdentity, QueueWisher,
    RequestStatus, StoredPasskey, TitleRequestRecord, TitleRequestSourceRecord, UserPreferences,
    UserRecord, UserRole, UserStatus,
};
use crate::wishlist_merge::apply_merged_sources;
use library_model::{holding_from_book, listing_from_book, WorkEnrichment};
use write_batches::WriteBatches;

/// Handle to the Bookclerk library database.
///
/// Async API over a SeaORM [`DatabaseConnection`]. `DatabaseConnection` is
/// cheaply cloneable (shared connection), so `LibraryStore` is `Clone`.
#[derive(Clone)]
pub struct LibraryStore {
    /// Shared SeaORM connection opened by the database plugin (SQLite / D1 / Postgres).
    db: DatabaseConnection,
    /// When set, named security methods run as one guest atomic batch
    /// instead of a local SeaORM transaction.
    atomic: Option<Arc<dyn AtomicTxnBackend>>,
    /// When set, granted guest SQL runs as one guest `executeAtomic`.
    typed_exec: Option<Arc<dyn crate::TypedAtomicExec>>,
    /// Negotiated guest `maxBinds` (defaults to D1's cap for in-process tests).
    max_binds: u32,
    /// Negotiated guest `maxStatements`.
    max_statements: u32,
    /// Negotiated guest `maxResultRows`.
    max_result_rows: u32,
    /// Negotiated guest `maxPayloadBytes`.
    max_payload_bytes: u32,
    /// Full negotiated capability advertisement (limits, schema flags, timing).
    caps: Arc<bookclerk_plugin_abi::DbCapabilities>,
    /// Leftover canonical `?` SQL execution target.
    ///
    /// Production plugin-host stays [`bookclerk_db_exec::SqlExecTarget::Canonical`]
    /// so the sqlite-shaped proxy never physically lowers. Native in-process
    /// tests set [`bookclerk_db_exec::SqlExecTarget::InProcess`].
    sql_exec: bookclerk_db_exec::SqlExecTarget,
    /// Where the last bounded delivery-expiry scan stopped, as the
    /// `(created_at, id)` of the last delivery it examined. The next scan
    /// resumes after it, so deliveries that cannot expire yet do not keep a
    /// later expired one from being reached. `None` starts from the oldest.
    /// Shared by clones; a new store starts over.
    expire_cursor: Arc<std::sync::Mutex<Option<(String, String)>>>,
}

impl std::fmt::Debug for LibraryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibraryStore").finish_non_exhaustive()
    }
}

impl LibraryStore {
    /// Wrap an already-opened (and migrated) SeaORM connection.
    ///
    /// Prefer the database plugin host (`open_library_store`) or, in tests,
    /// `bookclerk_plugin_database_sqlite::open_memory` — this crate does not
    /// open engine-specific connections.
    #[must_use]
    pub fn from_connection(db: DatabaseConnection) -> Self {
        Self {
            db,
            atomic: None,
            typed_exec: None,
            max_binds: bookclerk_plugin_abi::D1_MAX_BINDS,
            max_statements: bookclerk_plugin_abi::FIRST_PARTY_MAX_STATEMENTS,
            max_result_rows: 256,
            max_payload_bytes: 256 * 1024,
            caps: Arc::new(bookclerk_plugin_abi::DbCapabilities::advertised_d1()),
            sql_exec: bookclerk_db_exec::SqlExecTarget::Canonical,
            expire_cursor: Arc::default(),
        }
    }

    /// Leftover SQL is physically lowered by the adapter SDK (in-process tests).
    ///
    /// The SDK infers sqlite vs postgres from the opened connection. Production
    /// plugin-host must not call this — leftover SQL stays canonical on the proxy.
    #[cfg(test)]
    #[must_use]
    pub fn with_in_process_sql(mut self) -> Self {
        self.sql_exec = bookclerk_db_exec::SqlExecTarget::InProcess;
        self
    }

    /// True when leftover SQL is physically lowered on this store's connection.
    #[must_use]
    pub(crate) fn leftover_in_process(&self) -> bool {
        self.sql_exec.lowers()
    }

    /// Attach a guest atomic backend for named security operations.
    ///
    /// Hosts attach this for every database plugin. Tests that open a local
    /// connection leave it unset and run the same commands through
    /// [`crate::execute_db_atomic`].
    #[must_use]
    pub fn with_atomic_txn(mut self, backend: Arc<dyn AtomicTxnBackend>) -> Self {
        self.atomic = Some(backend);
        self
    }

    /// Attach a guest `executeAtomic` backend for granted job SQL.
    ///
    /// Hosts attach this for every database plugin so job sessions do not use
    /// the SeaORM proxy. Tests that open a local connection leave it unset.
    #[must_use]
    pub fn with_typed_exec(mut self, backend: Arc<dyn crate::TypedAtomicExec>) -> Self {
        self.typed_exec = Some(backend);
        self
    }

    /// Records the guest's negotiated bind cap for wake `IN (…)` chunking.
    #[must_use]
    pub fn with_max_binds(mut self, max_binds: u32) -> Self {
        self.max_binds = max_binds.max(bookclerk_plugin_abi::HOST_MIN_BINDS);
        let mut caps = (*self.caps).clone();
        caps.max_binds = self.max_binds;
        self.caps = Arc::new(caps);
        self
    }

    /// Records negotiated statement / result / payload caps from connect.
    #[must_use]
    pub fn with_plan_limits(
        mut self,
        max_statements: u32,
        max_result_rows: u32,
        max_payload_bytes: u32,
    ) -> Self {
        self.max_statements = max_statements.max(bookclerk_plugin_abi::HOST_MIN_STATEMENTS);
        self.max_result_rows = max_result_rows.max(1);
        self.max_payload_bytes = max_payload_bytes.max(1024);
        let mut caps = (*self.caps).clone();
        caps.max_statements = self.max_statements;
        caps.max_result_rows = self.max_result_rows;
        caps.max_payload_bytes = self.max_payload_bytes;
        self.caps = Arc::new(caps);
        self
    }

    /// Records the full negotiated [`bookclerk_plugin_abi::DbCapabilities`].
    #[must_use]
    pub fn with_db_capabilities(mut self, caps: bookclerk_plugin_abi::DbCapabilities) -> Self {
        self.max_binds = caps.max_binds.max(bookclerk_plugin_abi::HOST_MIN_BINDS);
        self.max_statements = caps
            .max_statements
            .max(bookclerk_plugin_abi::HOST_MIN_STATEMENTS);
        self.max_result_rows = caps.max_result_rows.max(1);
        self.max_payload_bytes = caps.max_payload_bytes.max(1024);
        self.caps = Arc::new(caps);
        self
    }

    /// Negotiated capability advertisement used for plan validation.
    #[must_use]
    pub fn db_capabilities(&self) -> &bookclerk_plugin_abi::DbCapabilities {
        self.caps.as_ref()
    }

    /// Runs guest-authored SQL after grammar, table-scope, and cap checks.
    ///
    /// Hosts inject this into `JobRunner.job` as the granted
    /// [`bookclerk_plugin_abi::GuestDatabase::execute`] path.
    /// Job sessions pass a host-issued [`GuestSqlPolicy`](bookclerk_plugin_abi::GuestSqlPolicy)
    /// (`books` by default).
    ///
    /// # Errors
    ///
    /// Returns [`bookclerk_plugin_abi::PluginError::invalid_params`] when the
    /// SQL is outside the guest grammar or allowlist, and an engine/transport
    /// error when the batch fails.
    pub async fn execute_guest_atomic(
        &self,
        req: bookclerk_plugin_abi::ExecuteRequest,
        policy: &bookclerk_plugin_abi::GuestSqlPolicy,
    ) -> std::result::Result<bookclerk_plugin_abi::ExecuteReply, bookclerk_plugin_abi::PluginError>
    {
        let db = self.db.clone();
        let caps = self.db_capabilities().clone();
        let typed_exec = self.typed_exec.clone();
        let exec_caps = caps.clone();
        crate::sql_plan::execute_guest_atomic_with(req, &caps, policy, move |envelope| {
            let db = db.clone();
            let caps = exec_caps.clone();
            let typed_exec = typed_exec.clone();
            async move {
                if let Some(exec) = typed_exec {
                    exec.execute_typed(envelope).await
                } else {
                    // In-process tests (no RPC adapter). Production hosts
                    // always inject [`crate::TypedAtomicExec`]. The adapter
                    // SDK infers sqlite vs postgres from `db`.
                    let deadline = (envelope.request.deadline_unix_ms > 0)
                        .then_some(envelope.request.deadline_unix_ms);
                    bookclerk_db_exec::execute_typed_envelope_on_connection(
                        &db,
                        &envelope,
                        bookclerk_db_exec::ExecCaps::from_capabilities(&caps),
                        bookclerk_db_exec::AtomicSession::from_deadline(deadline)
                            .with_type_env(crate::migrations::host_sql_type_env()),
                    )
                    .await
                    .map_err(plugin_err_from_db)
                }
            }
        })
        .await
    }

    /// Subscribers packed into one dispatch plan (receipt overhead and the
    /// live counter bump are ~13 statements).
    #[must_use]
    pub fn dispatch_chunk_size(&self) -> usize {
        if let Some(n) = crate::store::event_outbox::dispatch_chunk_override() {
            return n.max(1);
        }
        const OVERHEAD: u32 = 13;
        const BINDS_PER_INSERT: u32 = 9;
        let by_stmts = self.max_statements.saturating_sub(OVERHEAD).max(1);
        let by_binds = self.max_binds.saturating_sub(16) / BINDS_PER_INSERT;
        usize::try_from(by_stmts.min(by_binds.max(1))).unwrap_or(1)
    }

    /// Wake delivery page size derived from negotiated `maxBinds`.
    #[must_use]
    pub fn wake_page(&self) -> u64 {
        crate::sql_plan::wake_page_for_max_binds(self.max_binds)
    }

    /// Borrow the underlying SeaORM connection (e.g. for [`crate::secrets`]).
    #[must_use]
    pub fn connection(&self) -> &DatabaseConnection {
        &self.db
    }

    /// Alias for [`Self::connection`] — convenience name used by credential modules.
    #[must_use]
    pub fn db(&self) -> &DatabaseConnection {
        &self.db
    }

    /// Write-lock active owner rows so last-owner checks cannot race a concurrent
    /// demote/disable/delete (SQLite reserved lock / Postgres row locks).
    ///
    /// Used by the SeaORM `*_on` txn helpers kept for in-process tests.
    #[allow(dead_code)]
    async fn lock_active_owners<C: ConnectionTrait>(conn: &C) -> Result<()> {
        use sea_orm::sea_query::Expr;
        users::Entity::update_many()
            .col_expr(
                users::Column::UpdatedAt,
                Expr::col(users::Column::UpdatedAt),
            )
            .filter(users::Column::Role.eq(UserRole::Owner.as_str()))
            .filter(users::Column::Status.eq(UserStatus::Active.as_str()))
            .exec(conn)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Counts active `owner` users on `conn` (used by last-owner guards).
    #[allow(dead_code)]
    async fn count_active_owners_on<C: ConnectionTrait>(conn: &C) -> Result<u64> {
        users::Entity::find()
            .filter(users::Column::Role.eq(UserRole::Owner.as_str()))
            .filter(users::Column::Status.eq(UserStatus::Active.as_str()))
            .count(conn)
            .await
            .map_err(LibraryError::Orm)
    }

    /// Deletes Operator sessions minted via this user's elevate-to-operator flow.
    async fn delete_elevated_operator_sessions_for_user_on<C: ConnectionTrait>(
        conn: &C,
        user_id: i64,
    ) -> Result<u64> {
        let result = operator_sessions::Entity::delete_many()
            .filter(operator_sessions::Column::ElevatedFromUserId.eq(user_id))
            .exec(conn)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(result.rows_affected)
    }

    /// Revoke Operator sessions minted by this user's elevate-to-operator flow.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn delete_elevated_operator_sessions_for_user(&self, user_id: i64) -> Result<u64> {
        Self::delete_elevated_operator_sessions_for_user_on(&self.db, user_id).await
    }

    /// Upsert an account (updates `scan_enabled` on conflict).
    ///
    /// Accounts are keyed by `(plugin_key, account_id)`: the same store id may
    /// exist under two plugins. `source` is the owning plugin's display alias.
    /// Content sources should prefer [`crate::SourceScope::upsert_account`],
    /// which forces both.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn upsert_account(
        &self,
        plugin_key: &str,
        account_id: &str,
        marketplace: &str,
        label: Option<&str>,
        scan_enabled: bool,
        source: &str,
    ) -> Result<AccountRecord> {
        self.upsert_account_inner(
            plugin_key,
            account_id,
            marketplace,
            label,
            scan_enabled,
            source,
            true,
        )
        .await
    }

    /// Ensure an account row exists without overwriting `scan_enabled`.
    ///
    /// Content sources should prefer [`crate::SourceScope::ensure_account`].
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn ensure_account(
        &self,
        plugin_key: &str,
        account_id: &str,
        marketplace: &str,
        label: Option<&str>,
        source: &str,
    ) -> Result<AccountRecord> {
        self.upsert_account_inner(
            plugin_key,
            account_id,
            marketplace,
            label,
            true,
            source,
            false,
        )
        .await
    }

    /// Inserts or updates a store account; optionally overwrites `scan_enabled`.
    #[allow(clippy::too_many_arguments)]
    async fn upsert_account_inner(
        &self,
        plugin_key: &str,
        account_id: &str,
        marketplace: &str,
        label: Option<&str>,
        scan_enabled: bool,
        source: &str,
        update_scan_enabled: bool,
    ) -> Result<AccountRecord> {
        if plugin_key.is_empty() {
            return Err(LibraryError::Other(anyhow::anyhow!(
                "account `{account_id}` needs an owning plugin key"
            )));
        }
        let now = now_str();
        let existing = accounts::Entity::find()
            .filter(accounts::Column::PluginKey.eq(plugin_key))
            .filter(accounts::Column::AccountId.eq(account_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;

        if let Some(model) = existing {
            let mut am: accounts::ActiveModel = model.into();
            // The alias is display only; a renamed plugin keeps its rows.
            am.source = Set(source.to_string());
            am.marketplace = Set(marketplace.to_string());
            if let Some(label) = label {
                am.label = Set(Some(label.to_string()));
            }
            if update_scan_enabled {
                am.scan_enabled = Set(i64::from(scan_enabled));
            }
            am.updated_at = Set(now);
            let model = am.update(&self.db).await.map_err(LibraryError::Orm)?;
            return Ok(map_account(model));
        }

        let am = accounts::ActiveModel {
            id: NotSet,
            plugin_key: Set(plugin_key.to_string()),
            account_id: Set(account_id.to_string()),
            marketplace: Set(marketplace.to_string()),
            label: Set(label.map(str::to_string)),
            scan_enabled: Set(i64::from(scan_enabled)),
            source: Set(source.to_string()),
            connection_status: Set(String::from("active")),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        };
        let model = am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(map_account(model))
    }

    /// Remap `from` → `to` account ids within one plugin (books + account row).
    ///
    /// Used when classic Libation stored an email `AccountId` and a later
    /// login/scan discovers Audible `customer_id`.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn remap_account_id(&self, plugin_key: &str, from: &str, to: &str) -> Result<()> {
        if from == to {
            return Ok(());
        }
        let Some(from_acct) = accounts::Entity::find()
            .filter(accounts::Column::PluginKey.eq(plugin_key))
            .filter(accounts::Column::AccountId.eq(from))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(());
        };

        let to_acct = accounts::Entity::find()
            .filter(accounts::Column::PluginKey.eq(plugin_key))
            .filter(accounts::Column::AccountId.eq(to))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let now = now_str();

        match to_acct {
            None => {
                // Copy the account row under the new id.
                let am = accounts::ActiveModel {
                    id: NotSet,
                    plugin_key: Set(plugin_key.to_string()),
                    account_id: Set(to.to_string()),
                    marketplace: Set(from_acct.marketplace.clone()),
                    label: Set(from_acct.label.clone()),
                    scan_enabled: Set(from_acct.scan_enabled),
                    source: Set(from_acct.source.clone()),
                    connection_status: Set(from_acct.connection_status.clone()),
                    created_at: Set(from_acct.created_at.clone()),
                    updated_at: Set(now.clone()),
                };
                am.insert(&self.db).await.map_err(LibraryError::Orm)?;
            }
            Some(to_model) => {
                // Prefer the old label when the canonical row has none.
                if to_model.label.is_none() && from_acct.label.is_some() {
                    let mut am: accounts::ActiveModel = to_model.into();
                    am.label = Set(from_acct.label.clone());
                    am.updated_at = Set(now.clone());
                    am.update(&self.db).await.map_err(LibraryError::Orm)?;
                }
            }
        }

        // Move books that do not already exist under `to` (same product_id).
        let existing_keys: std::collections::HashSet<String> = books::Entity::find()
            .filter(books::Column::PluginKey.eq(plugin_key))
            .filter(books::Column::AccountId.eq(to))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(|b| b.product_id)
            .collect();

        let from_books = books::Entity::find()
            .filter(books::Column::PluginKey.eq(plugin_key))
            .filter(books::Column::AccountId.eq(from))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        for book in from_books {
            if existing_keys.contains(&book.product_id) {
                continue;
            }
            let mut am: books::ActiveModel = book.into();
            am.account_id = Set(to.to_string());
            am.updated_at = Set(now.clone());
            self.update_book_bumped(am).await?;
        }

        // Drop duplicate product rows left on the old id, then the account.
        let probe = books::Entity::find()
            .filter(books::Column::PluginKey.eq(plugin_key))
            .filter(books::Column::AccountId.eq(from));
        self.exec_bumped(
            books::Entity::delete_many()
                .filter(books::Column::PluginKey.eq(plugin_key))
                .filter(books::Column::AccountId.eq(from)),
            LiveTopic::Library,
            Bump::IfAny(sea_probe(&probe)?),
        )
        .await?;
        accounts::Entity::delete_many()
            .filter(accounts::Column::PluginKey.eq(plugin_key))
            .filter(accounts::Column::AccountId.eq(from))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Remap any existing alias account ids onto `canonical_id`, then upsert.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn reconcile_account_id(
        &self,
        plugin_key: &str,
        canonical_id: &str,
        aliases: &[&str],
        marketplace: &str,
        label: Option<&str>,
        scan_enabled: bool,
        source: &str,
    ) -> Result<AccountRecord> {
        for alias in aliases {
            if *alias != canonical_id {
                self.remap_account_id(plugin_key, alias, canonical_id)
                    .await?;
            }
        }
        self.upsert_account(
            plugin_key,
            canonical_id,
            marketplace,
            label,
            scan_enabled,
            source,
        )
        .await
    }

    /// Loads one account by `(plugin_key, account_id)`, if present.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn get_account(
        &self,
        plugin_key: &str,
        account_id: &str,
    ) -> Result<Option<AccountRecord>> {
        Ok(accounts::Entity::find()
            .filter(accounts::Column::PluginKey.eq(plugin_key))
            .filter(accounts::Column::AccountId.eq(account_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_account))
    }

    /// Every account row with this store id, across plugins.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails.
    pub async fn find_accounts_by_id(&self, account_id: &str) -> Result<Vec<AccountRecord>> {
        Ok(accounts::Entity::find()
            .filter(accounts::Column::AccountId.eq(account_id))
            .order_by_asc(accounts::Column::PluginKey)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_account)
            .collect())
    }

    /// Resolves one account by store id, failing closed when two plugins own
    /// that id.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Conflict`] when the id is ambiguous.
    pub async fn get_account_unique(&self, account_id: &str) -> Result<Option<AccountRecord>> {
        let mut rows = self.find_accounts_by_id(account_id).await?;
        match rows.len() {
            0 => Ok(None),
            1 => Ok(rows.pop()),
            _ => Err(ambiguous_account(account_id, &rows)),
        }
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
        Ok(accounts::Entity::find()
            .order_by_asc(accounts::Column::AccountId)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_account)
            .collect())
    }

    /// Count library identity rows (SQL `COUNT`, not a full fetch).
    ///
    /// Same cardinality as [`Self::count_accounts`]. Prefer this when the caller
    /// only needs a number for an operator-facing summary.
    ///
    /// A peer writer on the same SQLite file (TRUNCATE journal) can make this
    /// read return `SQLITE_BUSY`. The read is repeated until the lock clears
    /// or the attempt budget is spent.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails for a reason other
    /// than file-lock contention, and [`LibraryError::Unavailable`] when the
    /// lock is still held after the retries.
    pub async fn count_identities(&self) -> Result<i64> {
        lock_retry::retry_read_lock(|| async {
            let count = accounts::Entity::find()
                .count(&self.db)
                .await
                .map_err(LibraryError::from_db_err)?;
            Ok(count as i64)
        })
        .await
    }

    /// Count account rows (SQL `COUNT`, not a full fetch).
    ///
    /// Uses [`Self::count_identities`], including its wait for a peer SQLite lock.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails for a reason other
    /// than file-lock contention, and [`LibraryError::Unavailable`] when the
    /// lock is still held after the retries.
    pub async fn count_accounts(&self) -> Result<i64> {
        self.count_identities().await
    }

    /// Every account whose id or nickname (`label`) matches, case-insensitive.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn find_accounts(&self, identifier: &str) -> Result<Vec<AccountRecord>> {
        Ok(self
            .list_accounts()
            .await?
            .into_iter()
            .filter(|a| {
                a.account_id.eq_ignore_ascii_case(identifier)
                    || a.label
                        .as_ref()
                        .is_some_and(|l| l.eq_ignore_ascii_case(identifier))
            })
            .collect())
    }

    /// Resolve one account by id or nickname (`label`), case-insensitive.
    ///
    /// `plugin_key` narrows the match to one plugin. Without it, a needle that
    /// matches accounts of two plugins fails closed.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Conflict`] when the needle is ambiguous.
    pub async fn find_account(
        &self,
        identifier: &str,
        plugin_key: Option<&str>,
    ) -> Result<Option<AccountRecord>> {
        let mut rows = self.find_accounts(identifier).await?;
        if let Some(key) = plugin_key {
            rows.retain(|a| a.plugin_key == key);
        }
        match rows.len() {
            0 => Ok(None),
            1 => Ok(rows.pop()),
            _ => Err(ambiguous_account(identifier, &rows)),
        }
    }

    /// Toggle whether an account is included in automatic library scans.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn set_scan_enabled(
        &self,
        plugin_key: &str,
        account_id: &str,
        scan_enabled: bool,
    ) -> Result<()> {
        let model = accounts::Entity::find()
            .filter(accounts::Column::PluginKey.eq(plugin_key))
            .filter(accounts::Column::AccountId.eq(account_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound(account_id.into()))?;
        let mut am: accounts::ActiveModel = model.into();
        am.scan_enabled = Set(i64::from(scan_enabled));
        am.updated_at = Set(now_str());
        am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Mark bookstore credentials active again (after reconnect).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn mark_connection_active(&self, plugin_key: &str, account_id: &str) -> Result<()> {
        let model = accounts::Entity::find()
            .filter(accounts::Column::PluginKey.eq(plugin_key))
            .filter(accounts::Column::AccountId.eq(account_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound(account_id.into()))?;
        let mut am: accounts::ActiveModel = model.into();
        am.connection_status = Set(String::from("active"));
        am.scan_enabled = Set(1);
        am.updated_at = Set(now_str());
        am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Delete all `encrypted_secrets` rows for an account (source auth + Widevine CDM).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn delete_account_secrets(&self, plugin_key: &str, account_id: &str) -> Result<()> {
        crate::secrets::delete_secrets_for_account(&self.db, plugin_key, account_id).await
    }

    /// Mark bookstore credentials revoked without deleting the account or books.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn revoke_credentials(&self, plugin_key: &str, account_id: &str) -> Result<()> {
        let model = accounts::Entity::find()
            .filter(accounts::Column::PluginKey.eq(plugin_key))
            .filter(accounts::Column::AccountId.eq(account_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound(account_id.into()))?;
        let mut am: accounts::ActiveModel = model.into();
        am.scan_enabled = Set(0);
        am.connection_status = Set(String::from("revoked"));
        am.updated_at = Set(now_str());
        am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Create or fetch a portal identity for `(provider, external_user_id)`.
    ///
    /// Ensures a first-party [`UserRecord`] (role `member`) is linked via
    /// `portal_identities.user_id`.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn upsert_portal_identity(
        &self,
        provider: &str,
        external_user_id: &str,
        label: Option<&str>,
    ) -> Result<crate::models::PortalIdentity> {
        let existing = portal_identities::Entity::find()
            .filter(portal_identities::Column::Provider.eq(provider))
            .filter(portal_identities::Column::ExternalUserId.eq(external_user_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if let Some(model) = existing {
            let mut model = model;
            if label.is_some() && model.label.is_none() {
                let mut am: portal_identities::ActiveModel = model.clone().into();
                am.label = Set(label.map(str::to_string));
                model = am.update(&self.db).await.map_err(LibraryError::Orm)?;
            }
            if model.user_id.is_none() {
                model = self.bridge_portal_identity_to_user(model).await?;
            }
            return Ok(map_portal_identity(model));
        }
        let user = self.create_user(UserRole::Member, label, None).await?;
        let am = portal_identities::ActiveModel {
            id: NotSet,
            provider: Set(provider.to_string()),
            external_user_id: Set(external_user_id.to_string()),
            label: Set(label.map(str::to_string)),
            user_id: Set(Some(user.id)),
            created_at: Set(now_str()),
            picture_url: Set(None),
        };
        let model = am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(map_portal_identity(model))
    }

    /// Create a first-party user.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn create_user(
        &self,
        role: UserRole,
        display_name: Option<&str>,
        password_hash: Option<&str>,
    ) -> Result<UserRecord> {
        self.create_user_with_login(role, display_name, None, password_hash)
            .await
    }

    /// Create a first-party user with optional local login_name and email.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn create_user_with_login(
        &self,
        role: UserRole,
        display_name: Option<&str>,
        login_name: Option<&str>,
        password_hash: Option<&str>,
    ) -> Result<UserRecord> {
        self.create_user_with_profile(role, display_name, login_name, None, password_hash)
            .await
    }

    /// Create a first-party user with login, email, and optional password hash.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn create_user_with_profile(
        &self,
        role: UserRole,
        display_name: Option<&str>,
        login_name: Option<&str>,
        email: Option<&str>,
        password_hash: Option<&str>,
    ) -> Result<UserRecord> {
        let now = now_str();
        let am = users::ActiveModel {
            id: NotSet,
            role: Set(role.as_str().to_string()),
            status: Set(UserStatus::Active.as_str().to_string()),
            display_name: Set(display_name.map(str::to_string)),
            login_name: Set(normalize_login_name(login_name)),
            email: Set(crate::email::normalize_user_email(email)?),
            password_hash: Set(password_hash.map(str::to_string)),
            security_version: Set(0),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            last_seen_at: Set(None),
            avatar_source: Set(None),
            totp_enabled: Set(0),
        };
        let id = self
            .insert_bumped(users::Entity::insert(am), LiveTopic::Users)
            .await?;
        self.get_user(id)
            .await?
            .ok_or_else(|| LibraryError::NotFound(format!("user {id}")))
    }

    /// Writes a loaded title's changed columns and bumps the library topic
    /// in one batch, and answers the row as written.
    ///
    /// # Errors
    ///
    /// Returns the write or read error, or [`LibraryError::NotFound`] when
    /// the row is gone.
    pub(crate) async fn update_book_bumped(&self, am: books::ActiveModel) -> Result<books::Model> {
        let id = match &am.id {
            sea_orm::ActiveValue::Set(id) | sea_orm::ActiveValue::Unchanged(id) => *id,
            sea_orm::ActiveValue::NotSet => {
                return Err(LibraryError::Other(anyhow::anyhow!(
                    "a title update needs the row id"
                )))
            }
        };
        // A row loaded whole has every column, so the written row is known
        // without reading it back.
        let written = sea_orm::TryIntoModel::try_into_model(am.clone()).ok();
        let update = books::Entity::update(am)
            .validate()
            .map_err(LibraryError::Orm)?;
        let changed = self
            .exec_bumped(update, LiveTopic::Library, Bump::Always)
            .await?;
        match written {
            Some(model) if changed == 1 => Ok(model),
            _ => self.book_model_by_id(id).await,
        }
    }

    /// One title row by its row id.
    ///
    /// # Errors
    ///
    /// Returns the read error, or [`LibraryError::NotFound`] when it is gone.
    pub(crate) async fn book_model_by_id(&self, id: i64) -> Result<books::Model> {
        books::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound(format!("title row {id}")))
    }

    /// Writes a loaded user's changed columns and bumps the users topic in
    /// one batch, then reads the row back.
    ///
    /// # Errors
    ///
    /// Returns the write or read error, or [`LibraryError::NotFound`] when
    /// the user is gone.
    async fn update_user_bumped(&self, am: users::ActiveModel, id: i64) -> Result<UserRecord> {
        let update = users::Entity::update(am)
            .validate()
            .map_err(LibraryError::Orm)?;
        self.exec_bumped(update, LiveTopic::Users, Bump::Always)
            .await?;
        self.get_user(id)
            .await?
            .ok_or_else(|| LibraryError::NotFound(format!("user {id}")))
    }

    /// Look up a first-party user by id.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn get_user(&self, id: i64) -> Result<Option<UserRecord>> {
        Ok(users::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_user))
    }

    /// Look up by local login_name (case-insensitive).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn get_user_by_login_name(&self, login_name: &str) -> Result<Option<UserRecord>> {
        let Some(key) = normalize_login_name(Some(login_name)) else {
            return Ok(None);
        };
        Ok(users::Entity::find()
            .filter(users::Column::LoginName.eq(key))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_user))
    }

    /// Look up by email (case-insensitive).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn get_user_by_email(&self, email: &str) -> Result<Option<UserRecord>> {
        let Some(key) = crate::email::normalize_email_lookup(Some(email)) else {
            return Ok(None);
        };
        Ok(users::Entity::find()
            .filter(users::Column::Email.eq(key))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_user))
    }

    /// Raw password hash for verification (never expose via API).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn get_user_password_hash(&self, id: i64) -> Result<Option<String>> {
        Ok(users::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .and_then(|m| m.password_hash))
    }

    /// List all first-party users (admin tooling).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn list_users(&self) -> Result<Vec<UserRecord>> {
        Ok(users::Entity::find()
            .order_by_asc(users::Column::Id)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_user)
            .collect())
    }

    /// Presence, listening, and storefront links for administrator user lists.
    ///
    /// `online` means any non-expired portal session. `listening` is the newest
    /// unfinished progress row with `last_listened_at` within
    /// `listening_within` (typical UI window: ~30 minutes). Used by
    /// `GET /api/users`. Session activity (`last_active_at`) comes from
    /// unexpired `portal_sessions.last_used_at`. Durable last-seen lives on
    /// `users.last_seen_at` (survives logout) and is refreshed with session
    /// use at most once per minute.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn list_user_presence_extras(
        &self,
        listening_within: chrono::Duration,
    ) -> Result<std::collections::HashMap<i64, crate::models::UserPresenceExtras>> {
        use crate::models::{UserIntegrationHint, UserListeningHint, UserPresenceExtras};
        use std::collections::HashMap;

        let now = Utc::now();
        let now_s = now_str();
        let listening_cutoff = now - listening_within;

        let identities = portal_identities::Entity::find()
            .filter(portal_identities::Column::UserId.is_not_null())
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;

        let mut identity_to_user: HashMap<i64, i64> = HashMap::new();
        let mut user_identity_ids: HashMap<i64, Vec<i64>> = HashMap::new();
        for identity in &identities {
            let Some(user_id) = identity.user_id else {
                continue;
            };
            identity_to_user.insert(identity.id, user_id);
            user_identity_ids
                .entry(user_id)
                .or_default()
                .push(identity.id);
        }

        let mut out: HashMap<i64, UserPresenceExtras> = HashMap::new();
        for user_id in user_identity_ids.keys() {
            out.insert(
                *user_id,
                UserPresenceExtras {
                    online: false,
                    listening: None,
                    integrations: Vec::new(),
                    last_active_at: None,
                },
            );
        }

        let sessions = portal_sessions::Entity::find()
            .filter(portal_sessions::Column::ExpiresAt.gt(now_s.as_str()))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        for session in sessions {
            let Some(user_id) = identity_to_user.get(&session.identity_id).copied() else {
                continue;
            };
            let extras = out.entry(user_id).or_insert_with(|| UserPresenceExtras {
                online: false,
                listening: None,
                integrations: Vec::new(),
                last_active_at: None,
            });
            extras.online = true;
            let active = parse_dt_opt(session.last_used_at.as_deref())
                .or_else(|| parse_dt_opt(Some(session.created_at.as_str())));
            if let Some(at) = active {
                if extras.last_active_at.map(|prev| at > prev).unwrap_or(true) {
                    extras.last_active_at = Some(at);
                }
            }
        }

        let identity_ids: Vec<i64> = identity_to_user.keys().copied().collect();
        if !identity_ids.is_empty() {
            let links = account_links::Entity::find()
                .filter(account_links::Column::IdentityId.is_in(identity_ids.clone()))
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            let account_ids: Vec<String> = links.iter().map(|l| l.account_id.clone()).collect();
            let accounts = if account_ids.is_empty() {
                Vec::new()
            } else {
                accounts::Entity::find()
                    .filter(accounts::Column::AccountId.is_in(account_ids))
                    .all(&self.db)
                    .await
                    .map_err(LibraryError::Orm)?
            };
            let label_by_account: HashMap<(String, String), Option<String>> = accounts
                .into_iter()
                .map(|a| ((a.plugin_key, a.account_id), a.label))
                .collect();
            for link in links {
                let Some(user_id) = identity_to_user.get(&link.identity_id).copied() else {
                    continue;
                };
                let extras = out.entry(user_id).or_insert_with(|| UserPresenceExtras {
                    online: false,
                    listening: None,
                    integrations: Vec::new(),
                    last_active_at: None,
                });
                let hint = UserIntegrationHint {
                    source: link.source,
                    account_id: link.account_id.clone(),
                    label: label_by_account
                        .get(&(link.plugin_key.clone(), link.account_id.clone()))
                        .cloned()
                        .flatten(),
                };
                if !extras
                    .integrations
                    .iter()
                    .any(|i| i.source == hint.source && i.account_id == hint.account_id)
                {
                    extras.integrations.push(hint);
                }
            }

            let progress = listening_progress::Entity::find()
                .filter(listening_progress::Column::IdentityId.is_in(identity_ids))
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            // The work each user's latest hint is about.
            let mut listening_work: HashMap<i64, String> = HashMap::new();
            for row in progress {
                if row.is_finished != 0 {
                    continue;
                }
                let Some(identity_id) = row.identity_id else {
                    continue;
                };
                let Some(at) = parse_dt_opt(row.last_listened_at.as_deref()) else {
                    continue;
                };
                if at < listening_cutoff {
                    continue;
                }
                let Some(user_id) = identity_to_user.get(&identity_id).copied() else {
                    continue;
                };
                let extras = out.entry(user_id).or_insert_with(|| UserPresenceExtras {
                    online: false,
                    listening: None,
                    integrations: Vec::new(),
                    last_active_at: None,
                });
                let replace = extras
                    .listening
                    .as_ref()
                    .map(|prev| at > prev.last_listened_at)
                    .unwrap_or(true);
                if replace {
                    extras.listening = Some(UserListeningHint {
                        title: None,
                        provider: row.provider,
                        last_listened_at: at,
                    });
                    listening_work.insert(user_id, row.work_id);
                }
            }
            // The hint's title is its work's, read for every user at once.
            let work_ids: Vec<String> = listening_work
                .values()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            let works = self.works_by_ids(&work_ids).await?;
            for (user_id, work_id) in listening_work {
                if let Some(hint) = out.get_mut(&user_id).and_then(|e| e.listening.as_mut()) {
                    hint.title = works.get(&work_id).map(|w| w.title.clone());
                }
            }
        }

        Ok(out)
    }

    /// Delete a first-party user and personal data.
    ///
    /// Removes portal identities, account links, wishlist rows, claim tickets,
    /// sessions, and prefs for the user. Acquired library books are retained.
    /// Refuses to delete the last active owner. Publishes `wishlist_changed`
    /// (status `deleted`) for each open wish removed with the user, so
    /// Discover stops counting it for everyone else.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn delete_user(&self, id: i64) -> Result<()> {
        let identities: Vec<i64> = self
            .list_portal_identities_for_user(id)
            .await?
            .into_iter()
            .map(|i| i.id)
            .collect();
        let wishes = title_requests::Entity::find()
            .filter(title_requests::Column::IdentityId.is_in(identities))
            .filter(title_requests::Column::Status.eq(RequestStatus::Open.as_str()))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if let Some(atomic) = &self.atomic {
            atomic.delete_user(id).await?;
        } else {
            crate::db_atomic::delete_user(&self.db, id).await?;
        }
        for wish in wishes {
            self.publish_wishlist_changed(&wish.uuid, wish.identity_id, "deleted")
                .await;
        }
        Ok(())
    }

    /// Deletes a user and related portal rows inside an open transaction; refuses the last owner.
    #[allow(dead_code)]
    pub(crate) async fn delete_user_on<C: ConnectionTrait>(txn: &C, id: i64) -> Result<()> {
        Self::lock_active_owners(txn).await?;
        let model = users::Entity::find_by_id(id)
            .one(txn)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound(format!("user {id}")))?;
        let current = map_user(model);
        if matches!(current.role, UserRole::Owner)
            && matches!(current.status, UserStatus::Active)
            && Self::count_active_owners_on(txn).await? <= 1
        {
            return Err(LibraryError::LastOwner);
        }

        let identities = portal_identities::Entity::find()
            .filter(portal_identities::Column::UserId.eq(id))
            .all(txn)
            .await
            .map_err(LibraryError::Orm)?;

        for identity in &identities {
            account_links::Entity::delete_many()
                .filter(account_links::Column::IdentityId.eq(identity.id))
                .exec(txn)
                .await
                .map_err(LibraryError::Orm)?;
            title_requests::Entity::delete_many()
                .filter(title_requests::Column::IdentityId.eq(identity.id))
                .exec(txn)
                .await
                .map_err(LibraryError::Orm)?;
            claim_tickets::Entity::delete_many()
                .filter(claim_tickets::Column::IdentityId.eq(identity.id))
                .exec(txn)
                .await
                .map_err(LibraryError::Orm)?;
            portal_sessions::Entity::delete_many()
                .filter(portal_sessions::Column::IdentityId.eq(identity.id))
                .exec(txn)
                .await
                .map_err(LibraryError::Orm)?;
            listening_progress::Entity::delete_many()
                .filter(listening_progress::Column::IdentityId.eq(identity.id))
                .exec(txn)
                .await
                .map_err(LibraryError::Orm)?;
            user_preferences::Entity::delete_many()
                .filter(user_preferences::Column::IdentityId.eq(identity.id))
                .exec(txn)
                .await
                .map_err(LibraryError::Orm)?;
        }

        // Elevated origin must not become a normal Operator session.
        Self::delete_elevated_operator_sessions_for_user_on(txn, id).await?;
        // Impersonation pointer: clear only (session remains a real Operator).
        {
            use sea_orm::sea_query::Expr;
            operator_sessions::Entity::update_many()
                .col_expr(
                    operator_sessions::Column::ImpersonatingUserId,
                    Expr::value(Option::<i64>::None),
                )
                .filter(operator_sessions::Column::ImpersonatingUserId.eq(id))
                .exec(txn)
                .await
                .map_err(LibraryError::Orm)?;
        }

        oidc_refresh_tokens::Entity::delete_many()
            .filter(oidc_refresh_tokens::Column::UserId.eq(id))
            .exec(txn)
            .await
            .map_err(LibraryError::Orm)?;
        oidc_auth_codes::Entity::delete_many()
            .filter(oidc_auth_codes::Column::UserId.eq(id))
            .exec(txn)
            .await
            .map_err(LibraryError::Orm)?;
        webauthn_credentials::Entity::delete_many()
            .filter(webauthn_credentials::Column::UserId.eq(id))
            .exec(txn)
            .await
            .map_err(LibraryError::Orm)?;
        webauthn_challenges::Entity::delete_many()
            .filter(webauthn_challenges::Column::UserId.eq(id))
            .exec(txn)
            .await
            .map_err(LibraryError::Orm)?;
        encrypted_secrets::Entity::delete_many()
            .filter(encrypted_secrets::Column::Kind.eq(crate::secret_kind::TOTP))
            .filter(encrypted_secrets::Column::AccountType.eq(crate::secret_account_type::USER))
            .filter(encrypted_secrets::Column::AccountId.eq(id.to_string()))
            .exec(txn)
            .await
            .map_err(LibraryError::Orm)?;
        oidc_rp_states::Entity::delete_many()
            .filter(oidc_rp_states::Column::UserId.eq(id))
            .exec(txn)
            .await
            .map_err(LibraryError::Orm)?;
        crate::entities::user_title_state::Entity::delete_many()
            .filter(crate::entities::user_title_state::Column::UserId.eq(id))
            .exec(txn)
            .await
            .map_err(LibraryError::Orm)?;

        portal_identities::Entity::delete_many()
            .filter(portal_identities::Column::UserId.eq(id))
            .exec(txn)
            .await
            .map_err(LibraryError::Orm)?;

        user_preferences::Entity::delete_many()
            .filter(user_preferences::Column::SubjectKey.eq(user_prefs_key(id)))
            .exec(txn)
            .await
            .map_err(LibraryError::Orm)?;

        users::Entity::delete_by_id(id)
            .exec(txn)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Set user status (`active` / `disabled`).
    ///
    /// Refuses to disable the last active owner.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn set_user_status(&self, id: i64, status: UserStatus) -> Result<UserRecord> {
        if let Some(atomic) = &self.atomic {
            return atomic.set_user_status(id, status).await;
        }
        crate::db_atomic::set_user_status(&self.db, id, status).await
    }

    /// Sets user status inside an open transaction; disabling the last owner fails.
    #[allow(dead_code)]
    pub(crate) async fn set_user_status_on<C: ConnectionTrait>(
        txn: &C,
        id: i64,
        status: UserStatus,
    ) -> Result<UserRecord> {
        Self::lock_active_owners(txn).await?;
        let model = users::Entity::find_by_id(id)
            .one(txn)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound(format!("user {id}")))?;
        let current = map_user(model.clone());
        if matches!(status, UserStatus::Disabled)
            && matches!(current.role, UserRole::Owner)
            && matches!(current.status, UserStatus::Active)
            && Self::count_active_owners_on(txn).await? <= 1
        {
            return Err(LibraryError::LastOwner);
        }
        let mut am: users::ActiveModel = model.into();
        am.status = Set(status.as_str().to_string());
        am.updated_at = Set(now_str());
        let model = am.update(txn).await.map_err(LibraryError::Orm)?;
        if matches!(status, UserStatus::Disabled) {
            Self::delete_elevated_operator_sessions_for_user_on(txn, id).await?;
        }
        Ok(map_user(model))
    }

    /// Set display name (does not bump security_version).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn set_user_display_name(
        &self,
        id: i64,
        display_name: Option<&str>,
    ) -> Result<UserRecord> {
        let model = users::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound(format!("user {id}")))?;
        let mut am: users::ActiveModel = model.into();
        am.display_name = Set(display_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string));
        am.updated_at = Set(now_str());
        self.update_user_bumped(am, id).await
    }

    /// Set or clear Argon2id password hash; bumps `security_version`.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn set_user_password_hash(
        &self,
        id: i64,
        password_hash: Option<&str>,
    ) -> Result<UserRecord> {
        if let Some(atomic) = &self.atomic {
            return atomic.set_user_password_hash(id, password_hash).await;
        }
        crate::db_atomic::set_user_password_hash(&self.db, id, password_hash).await
    }

    /// Sets or clears the Argon2id password hash inside an open transaction and bumps `security_version`.
    #[allow(dead_code)]
    pub(crate) async fn set_user_password_hash_on<C: ConnectionTrait>(
        txn: &C,
        id: i64,
        password_hash: Option<&str>,
    ) -> Result<UserRecord> {
        let model = users::Entity::find_by_id(id)
            .one(txn)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound(format!("user {id}")))?;
        let next_sv = model.security_version.saturating_add(1);
        let mut am: users::ActiveModel = model.into();
        am.password_hash = Set(password_hash.map(str::to_string));
        am.security_version = Set(next_sv);
        am.updated_at = Set(now_str());
        let model = am.update(txn).await.map_err(LibraryError::Orm)?;
        Self::delete_elevated_operator_sessions_for_user_on(txn, id).await?;
        Ok(map_user(model))
    }

    /// Set login_name (unique); does not bump security_version.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn set_user_login_name(
        &self,
        id: i64,
        login_name: Option<&str>,
    ) -> Result<UserRecord> {
        let model = users::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound(format!("user {id}")))?;
        let mut am: users::ActiveModel = model.into();
        am.login_name = Set(normalize_login_name(login_name));
        am.updated_at = Set(now_str());
        self.update_user_bumped(am, id).await
    }

    /// Set contact email (unique when set); does not bump security_version.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn set_user_email(&self, id: i64, email: Option<&str>) -> Result<UserRecord> {
        let model = users::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound(format!("user {id}")))?;
        let mut am: users::ActiveModel = model.into();
        am.email = Set(crate::email::normalize_user_email(email)?);
        am.updated_at = Set(now_str());
        self.update_user_bumped(am, id).await
    }

    /// Set the user's picture source (`monogram` / `gravatar` / `upload` / `sso:{id}` / auto).
    ///
    /// `None` or empty stores auto-resolve (upload, then last-used SSO, then Gravatar).
    ///
    /// # Errors
    ///
    /// Returns an error when the user is missing or the write fails.
    pub async fn set_user_avatar_source(
        &self,
        id: i64,
        source: Option<&str>,
    ) -> Result<UserRecord> {
        let model = users::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound(format!("user {id}")))?;
        let mut am: users::ActiveModel = model.into();
        am.avatar_source = Set(source
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string));
        am.updated_at = Set(now_str());
        self.update_user_bumped(am, id).await
    }

    /// Store or clear the HTTPS picture URL on a portal identity.
    ///
    /// Missing identities are ignored so callers can run this after a failed
    /// lookup without an extra round-trip.
    ///
    /// # Errors
    ///
    /// Returns an error when the write fails.
    pub async fn set_portal_identity_picture(
        &self,
        provider: &str,
        external_user_id: &str,
        picture_url: Option<&str>,
    ) -> Result<()> {
        let Some(existing) = self.get_portal_identity(provider, external_user_id).await? else {
            return Ok(());
        };
        let next = picture_url
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if existing.picture_url == next {
            return Ok(());
        }
        let model = portal_identities::Entity::find_by_id(existing.id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound("portal identity".into()))?;
        let mut am: portal_identities::ActiveModel = model.into();
        am.picture_url = Set(next);
        am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// IdP pictures linked to `user_id`, with last portal-session activity.
    ///
    /// Identities without a `picture_url` are omitted. `last_used_at` is the
    /// latest session `last_used_at` or `created_at` for that identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails.
    pub async fn list_user_sso_pictures(
        &self,
        user_id: i64,
    ) -> Result<Vec<crate::models::UserSsoPicture>> {
        let identities = portal_identities::Entity::find()
            .filter(portal_identities::Column::UserId.eq(user_id))
            .order_by_asc(portal_identities::Column::Id)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let mut out = Vec::new();
        for model in identities {
            let Some(picture_url) = model
                .picture_url
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
            else {
                continue;
            };
            let sessions = portal_sessions::Entity::find()
                .filter(portal_sessions::Column::IdentityId.eq(model.id))
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            let last_used_at = sessions
                .iter()
                .filter_map(|s| s.last_used_at.as_deref().or(Some(s.created_at.as_str())))
                .max()
                .map(parse_dt);
            out.push(crate::models::UserSsoPicture {
                identity_id: model.id,
                provider: model.provider,
                picture_url,
                last_used_at,
            });
        }
        Ok(out)
    }

    /// Delete all portal sessions for identities linked to `user_id`.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn delete_portal_sessions_for_user(&self, user_id: i64) -> Result<u64> {
        let identities = portal_identities::Entity::find()
            .filter(portal_identities::Column::UserId.eq(user_id))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let mut total = 0u64;
        for identity in identities {
            let result = portal_sessions::Entity::delete_many()
                .filter(portal_sessions::Column::IdentityId.eq(identity.id))
                .exec(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            total = total.saturating_add(result.rows_affected);
        }
        Ok(total)
    }

    /// Insert a user invite (store hash only).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn insert_user_invite(
        &self,
        token_hash: &str,
        role: UserRole,
        login_name: Option<&str>,
        display_name: Option<&str>,
        expires_at: chrono::DateTime<Utc>,
        created_by: &str,
    ) -> Result<crate::models::UserInviteRecord> {
        let am = user_invites::ActiveModel {
            id: NotSet,
            token_hash: Set(token_hash.to_string()),
            role: Set(role.as_str().to_string()),
            login_name: Set(normalize_login_name(login_name)),
            display_name: Set(display_name.map(str::to_string)),
            expires_at: Set(expires_at.to_rfc3339()),
            redeemed_at: Set(None),
            created_by: Set(created_by.to_string()),
            created_at: Set(now_str()),
        };
        let model = am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(map_user_invite(model))
    }

    /// Atomically redeem a user invite.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn redeem_user_invite(
        &self,
        token_hash: &str,
    ) -> Result<crate::models::UserInviteRecord> {
        use sea_orm::sea_query::Expr;

        let now = now_str();
        let result = user_invites::Entity::update_many()
            .col_expr(user_invites::Column::RedeemedAt, Expr::value(now.clone()))
            .filter(user_invites::Column::TokenHash.eq(token_hash))
            .filter(user_invites::Column::RedeemedAt.is_null())
            .filter(user_invites::Column::ExpiresAt.gt(now))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if result.rows_affected != 1 {
            return Err(LibraryError::Other(anyhow::anyhow!(
                "invite invalid, expired, or already redeemed"
            )));
        }
        let model = user_invites::Entity::find()
            .filter(user_invites::Column::TokenHash.eq(token_hash))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::Other(anyhow::anyhow!("invite missing after redeem")))?;
        Ok(map_user_invite(model))
    }

    /// Set user role (`owner` / `administrator` / `member`).
    ///
    /// Refuses to demote the last active owner.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn set_user_role(&self, id: i64, role: UserRole) -> Result<UserRecord> {
        if let Some(atomic) = &self.atomic {
            return atomic.set_user_role(id, role).await;
        }
        crate::db_atomic::set_user_role(&self.db, id, role).await
    }

    /// Sets user role inside an open transaction; demoting the last active owner fails.
    #[allow(dead_code)]
    pub(crate) async fn set_user_role_on<C: ConnectionTrait>(
        txn: &C,
        id: i64,
        role: UserRole,
    ) -> Result<UserRecord> {
        Self::lock_active_owners(txn).await?;
        let model = users::Entity::find_by_id(id)
            .one(txn)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound(format!("user {id}")))?;
        let current = map_user(model.clone());
        if !matches!(role, UserRole::Owner)
            && matches!(current.role, UserRole::Owner)
            && matches!(current.status, UserStatus::Active)
            && Self::count_active_owners_on(txn).await? <= 1
        {
            return Err(LibraryError::LastOwner);
        }
        let role_changed = current.role != role;
        let mut am: users::ActiveModel = model.into();
        am.role = Set(role.as_str().to_string());
        am.updated_at = Set(now_str());
        let model = am.update(txn).await.map_err(LibraryError::Orm)?;
        if role_changed {
            Self::delete_elevated_operator_sessions_for_user_on(txn, id).await?;
        }
        Ok(map_user(model))
    }

    /// Count administrators (includes disabled).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn count_administrators(&self) -> Result<u64> {
        users::Entity::find()
            .filter(users::Column::Role.eq(UserRole::Administrator.as_str()))
            .count(&self.db)
            .await
            .map_err(LibraryError::Orm)
    }

    /// Count administrators with status `active`.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn count_active_administrators(&self) -> Result<u64> {
        users::Entity::find()
            .filter(users::Column::Role.eq(UserRole::Administrator.as_str()))
            .filter(users::Column::Status.eq(UserStatus::Active.as_str()))
            .count(&self.db)
            .await
            .map_err(LibraryError::Orm)
    }

    /// Count owners (includes disabled) — bootstrap / inventory.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn count_owners(&self) -> Result<u64> {
        users::Entity::find()
            .filter(users::Column::Role.eq(UserRole::Owner.as_str()))
            .count(&self.db)
            .await
            .map_err(LibraryError::Orm)
    }

    /// Count owners with status `active` (last-owner demote/disable/delete guard).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn count_active_owners(&self) -> Result<u64> {
        users::Entity::find()
            .filter(users::Column::Role.eq(UserRole::Owner.as_str()))
            .filter(users::Column::Status.eq(UserStatus::Active.as_str()))
            .count(&self.db)
            .await
            .map_err(LibraryError::Orm)
    }

    /// Backfill `portal_identities.user_id` and remap prefs `portal:{id}` → `user:{id}`.
    ///
    /// Safe to call repeatedly after migrations. Does not link CLI-created store
    /// accounts that have no `account_links` (avoids widening access).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn ensure_users_bridged(&self) -> Result<usize> {
        let orphans = portal_identities::Entity::find()
            .filter(portal_identities::Column::UserId.is_null())
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let mut bridged = 0usize;
        for model in orphans {
            self.bridge_portal_identity_to_user(model).await?;
            bridged += 1;
        }
        // Remap legacy portal prefs keys when identity already has a user.
        let prefs = user_preferences::Entity::find()
            .filter(user_preferences::Column::SubjectKey.like("portal:%"))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        for pref in prefs {
            let Some(identity_id) = pref.identity_id else {
                continue;
            };
            let Some(identity) = portal_identities::Entity::find_by_id(identity_id)
                .one(&self.db)
                .await
                .map_err(LibraryError::Orm)?
            else {
                continue;
            };
            let Some(user_id) = identity.user_id else {
                continue;
            };
            let new_key = crate::models::user_prefs_key(user_id);
            if pref.subject_key == new_key {
                continue;
            }
            // Skip if target key already exists.
            if user_preferences::Entity::find()
                .filter(user_preferences::Column::SubjectKey.eq(&new_key))
                .one(&self.db)
                .await
                .map_err(LibraryError::Orm)?
                .is_some()
            {
                continue;
            }
            let mut am: user_preferences::ActiveModel = pref.into();
            am.subject_key = Set(new_key);
            am.update(&self.db).await.map_err(LibraryError::Orm)?;
        }
        Ok(bridged)
    }

    /// Creates a member user and attaches it when the portal identity has no `user_id` yet.
    async fn bridge_portal_identity_to_user(
        &self,
        model: portal_identities::Model,
    ) -> Result<portal_identities::Model> {
        if model.user_id.is_some() {
            return Ok(model);
        }
        let user = self
            .create_user(UserRole::Member, model.label.as_deref(), None)
            .await?;
        let mut am: portal_identities::ActiveModel = model.into();
        am.user_id = Set(Some(user.id));
        am.update(&self.db).await.map_err(LibraryError::Orm)
    }

    /// Ensure a `local` portal identity exists for a first-party user.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn ensure_local_portal_identity(
        &self,
        user_id: i64,
        label: Option<&str>,
    ) -> Result<crate::models::PortalIdentity> {
        let external = format!("user:{user_id}");
        if let Some(existing) = self.get_portal_identity("local", &external).await? {
            return Ok(existing);
        }
        let am = portal_identities::ActiveModel {
            id: NotSet,
            provider: Set(String::from("local")),
            external_user_id: Set(external),
            label: Set(label.map(str::to_string)),
            user_id: Set(Some(user_id)),
            created_at: Set(now_str()),
            picture_url: Set(None),
        };
        let model = am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(map_portal_identity(model))
    }

    /// Loads a portal identity by primary key, if present.
    ///
    /// # Arguments
    ///
    /// * `provider` - External provider id.
    /// * `external_user_id` - User id at the external provider.
    ///
    /// # Returns
    ///
    /// `Result<Option<crate::models::PortalIdentity>>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn get_portal_identity(
        &self,
        provider: &str,
        external_user_id: &str,
    ) -> Result<Option<crate::models::PortalIdentity>> {
        Ok(portal_identities::Entity::find()
            .filter(portal_identities::Column::Provider.eq(provider))
            .filter(portal_identities::Column::ExternalUserId.eq(external_user_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_portal_identity))
    }

    /// Look up portal identity by row id.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn get_portal_identity_by_id(
        &self,
        id: i64,
    ) -> Result<Option<crate::models::PortalIdentity>> {
        Ok(portal_identities::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_portal_identity))
    }

    /// Loads portal identities keyed by id (missing ids are omitted).
    ///
    /// # Arguments
    ///
    /// * `ids` - Portal identity primary keys to fetch.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails.
    async fn portal_identities_by_ids(
        &self,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, PortalIdentity>> {
        let mut unique = ids.to_vec();
        unique.sort_unstable();
        unique.dedup();
        if unique.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let rows = portal_identities::Entity::find()
            .filter(portal_identities::Column::Id.is_in(unique))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(rows
            .into_iter()
            .map(|m| {
                let mapped = map_portal_identity(m);
                (mapped.id, mapped)
            })
            .collect())
    }

    /// Loads first-party users keyed by id (missing ids are omitted).
    ///
    /// # Arguments
    ///
    /// * `ids` - First-party user primary keys to fetch.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails.
    async fn users_by_ids(
        &self,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, UserRecord>> {
        let mut unique = ids.to_vec();
        unique.sort_unstable();
        unique.dedup();
        if unique.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let rows = users::Entity::find()
            .filter(users::Column::Id.is_in(unique))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(rows
            .into_iter()
            .map(|m| {
                let mapped = map_user(m);
                (mapped.id, mapped)
            })
            .collect())
    }

    /// Insert a claim ticket (store only the hash).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn insert_claim_ticket(
        &self,
        token_hash: &str,
        identity_id: Option<i64>,
        expires_at: chrono::DateTime<Utc>,
        created_by: &str,
    ) -> Result<crate::models::ClaimTicketRecord> {
        let am = claim_tickets::ActiveModel {
            id: NotSet,
            token_hash: Set(token_hash.to_string()),
            identity_id: Set(identity_id),
            expires_at: Set(expires_at.to_rfc3339()),
            redeemed_at: Set(None),
            created_by: Set(created_by.to_string()),
            created_at: Set(now_str()),
        };
        let model = am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(map_claim_ticket(model))
    }

    /// Loads a claim ticket row by its token hash, if present.
    ///
    /// # Arguments
    ///
    /// * `token_hash` - SHA-256 hex digest of the opaque token.
    ///
    /// # Returns
    ///
    /// `Result<Option<crate::models::ClaimTicketRecord>>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn get_claim_ticket_by_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<crate::models::ClaimTicketRecord>> {
        Ok(claim_tickets::Entity::find()
            .filter(claim_tickets::Column::TokenHash.eq(token_hash))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_claim_ticket))
    }

    /// List unredeemed, unexpired claim tickets (newest first).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn list_open_claim_tickets(&self) -> Result<Vec<crate::models::ClaimTicketRecord>> {
        let now = now_str();
        Ok(claim_tickets::Entity::find()
            .filter(claim_tickets::Column::RedeemedAt.is_null())
            .filter(claim_tickets::Column::ExpiresAt.gt(now))
            .order_by_desc(claim_tickets::Column::Id)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_claim_ticket)
            .collect())
    }

    /// Atomically mark a claim ticket redeemed.
    ///
    /// Uses a single `UPDATE … WHERE redeemed_at IS NULL AND expires_at > now` so
    /// concurrent redeemers cannot both succeed.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn redeem_claim_ticket(
        &self,
        token_hash: &str,
    ) -> Result<crate::models::ClaimTicketRecord> {
        use sea_orm::sea_query::Expr;

        let now = now_str();
        let result = claim_tickets::Entity::update_many()
            .col_expr(claim_tickets::Column::RedeemedAt, Expr::value(now.clone()))
            .filter(claim_tickets::Column::TokenHash.eq(token_hash))
            .filter(claim_tickets::Column::RedeemedAt.is_null())
            .filter(claim_tickets::Column::ExpiresAt.gt(now))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if result.rows_affected != 1 {
            return Err(LibraryError::Other(anyhow::anyhow!(
                "claim ticket invalid, expired, or already redeemed"
            )));
        }
        self.get_claim_ticket_by_hash(token_hash)
            .await?
            .ok_or_else(|| {
                LibraryError::Other(anyhow::anyhow!("claim ticket missing after atomic redeem"))
            })
    }

    /// Consume a claim ticket, optionally set an unset local password, and mint
    /// a portal session in one transaction.
    ///
    /// `new_password_hash` is assigned only while the ticket-bound local user
    /// still has no password. A failed consume rolls back password and session
    /// writes. Missing password for an unset local user aborts without consuming
    /// the ticket.
    ///
    /// # Arguments
    ///
    /// * `token_hash` - SHA-256 hex digest of the claim ticket.
    /// * `session_hash` - SHA-256 hex digest of the new portal session token.
    /// * `expires_at` - RFC 3339 expiry for the minted session.
    /// * `client` - Optional client metadata stored on the session row.
    /// * `new_password_hash` - Argon2id hash to set when the local user has none.
    /// * `password_fingerprint` - Stable HMAC of the plaintext password for
    ///   atomic-execute idempotency; ignored by the mutation SQL.
    ///
    /// # Returns
    ///
    /// The portal identity bound to the consumed ticket.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the ticket is invalid/consumed, a required
    /// invite password is missing, or the database operation fails.
    pub async fn redeem_claim_ticket_to_session(
        &self,
        token_hash: &str,
        session_hash: &str,
        expires_at: chrono::DateTime<Utc>,
        client: Option<&crate::SessionClientInfo>,
        new_password_hash: Option<&str>,
        password_fingerprint: Option<&str>,
    ) -> Result<crate::models::PortalIdentity> {
        if let Some(atomic) = &self.atomic {
            return atomic
                .redeem_claim_ticket_to_session(
                    token_hash,
                    session_hash,
                    expires_at,
                    client,
                    new_password_hash,
                    password_fingerprint,
                )
                .await;
        }
        crate::db_atomic::redeem_claim_ticket_to_session(
            &self.db,
            token_hash,
            session_hash,
            expires_at,
            client,
            new_password_hash,
            password_fingerprint,
        )
        .await
    }

    /// Consumes an unredeemed, unexpired claim ticket and mints a portal session in one transaction.
    #[allow(dead_code)]
    pub(crate) async fn redeem_claim_ticket_to_session_on<C: ConnectionTrait>(
        txn: &C,
        token_hash: &str,
        session_hash: &str,
        expires_at: chrono::DateTime<Utc>,
        client: Option<&crate::SessionClientInfo>,
        new_password_hash: Option<&str>,
    ) -> Result<crate::models::PortalIdentity> {
        use sea_orm::sea_query::Expr;

        let now = now_str();
        let consumed = claim_tickets::Entity::update_many()
            .col_expr(claim_tickets::Column::RedeemedAt, Expr::value(now.clone()))
            .filter(claim_tickets::Column::TokenHash.eq(token_hash))
            .filter(claim_tickets::Column::RedeemedAt.is_null())
            .filter(claim_tickets::Column::ExpiresAt.gt(now.clone()))
            .exec(txn)
            .await
            .map_err(LibraryError::Orm)?;
        if consumed.rows_affected != 1 {
            return Err(LibraryError::Other(anyhow::anyhow!(
                "claim ticket invalid, expired, or already redeemed"
            )));
        }
        let ticket = claim_tickets::Entity::find()
            .filter(claim_tickets::Column::TokenHash.eq(token_hash))
            .one(txn)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| {
                LibraryError::Other(anyhow::anyhow!("claim ticket missing after atomic redeem"))
            })?;
        let identity_id = ticket.identity_id.ok_or_else(|| {
            LibraryError::Other(anyhow::anyhow!("claim ticket is not bound to an identity"))
        })?;
        let identity_model = portal_identities::Entity::find_by_id(identity_id)
            .one(txn)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| {
                LibraryError::Other(anyhow::anyhow!("portal identity missing for claim ticket"))
            })?;
        let identity = map_portal_identity(identity_model);
        if identity.provider == "local" {
            if let Some(user_id) = identity.user_id {
                let user = users::Entity::find_by_id(user_id)
                    .one(txn)
                    .await
                    .map_err(LibraryError::Orm)?
                    .ok_or_else(|| LibraryError::NotFound(format!("user {user_id}")))?;
                if user.password_hash.is_none() {
                    let Some(hash) = new_password_hash else {
                        return Err(LibraryError::Other(anyhow::anyhow!(
                            "password required — set a password to finish claim login"
                        )));
                    };
                    let next_sv = user.security_version.saturating_add(1);
                    let mut am: users::ActiveModel = user.into();
                    am.password_hash = Set(Some(hash.to_string()));
                    am.security_version = Set(next_sv);
                    am.updated_at = Set(now_str());
                    am.update(txn).await.map_err(LibraryError::Orm)?;
                    Self::delete_elevated_operator_sessions_for_user_on(txn, user_id).await?;
                }
            }
        }
        let session_now = now_str();
        let session = portal_sessions::ActiveModel {
            id: NotSet,
            token_hash: Set(session_hash.to_string()),
            identity_id: Set(identity.id),
            expires_at: Set(expires_at.to_rfc3339()),
            created_at: Set(session_now.clone()),
            last_used_at: Set(Some(session_now.clone())),
            user_agent: Set(client.and_then(|c| c.user_agent.clone())),
            device_type: Set(client.map(|c| c.device_type.clone())),
            client_label: Set(client.map(|c| c.client_label.clone())),
        };
        session.insert(txn).await.map_err(LibraryError::Orm)?;
        if let Some(user_id) = identity.user_id {
            Self::set_user_last_seen_on(txn, user_id, &session_now).await?;
        }
        Ok(identity)
    }

    /// Create a portal session (hash only).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn insert_portal_session(
        &self,
        token_hash: &str,
        identity_id: i64,
        expires_at: chrono::DateTime<Utc>,
    ) -> Result<()> {
        self.insert_portal_session_with_client(token_hash, identity_id, expires_at, None)
            .await
    }

    /// Create a portal session with optional client metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn insert_portal_session_with_client(
        &self,
        token_hash: &str,
        identity_id: i64,
        expires_at: chrono::DateTime<Utc>,
        client: Option<&crate::SessionClientInfo>,
    ) -> Result<()> {
        let now = now_str();
        let am = portal_sessions::ActiveModel {
            id: NotSet,
            token_hash: Set(token_hash.to_string()),
            identity_id: Set(identity_id),
            expires_at: Set(expires_at.to_rfc3339()),
            created_at: Set(now.clone()),
            last_used_at: Set(Some(now.clone())),
            user_agent: Set(client.and_then(|c| c.user_agent.clone())),
            device_type: Set(client.map(|c| c.device_type.clone())),
            client_label: Set(client.map(|c| c.client_label.clone())),
        };
        am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Self::set_last_seen_for_identity_on(&self.db, identity_id, &now).await?;
        Ok(())
    }

    /// Delete a portal session by token hash (logout / revoke).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn delete_portal_session(&self, token_hash: &str) -> Result<bool> {
        let result = portal_sessions::Entity::delete_many()
            .filter(portal_sessions::Column::TokenHash.eq(token_hash))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(result.rows_affected > 0)
    }

    /// List portal sessions for an identity (id, created_at, expires_at).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn list_portal_sessions_for_identity(
        &self,
        identity_id: i64,
    ) -> Result<Vec<(i64, String, String)>> {
        Ok(self
            .list_portal_session_records_for_identity(identity_id)
            .await?
            .into_iter()
            .map(|r| (r.id, r.created_at, r.expires_at))
            .collect())
    }

    /// List portal sessions with client metadata for session management UIs.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn list_portal_session_records_for_identity(
        &self,
        identity_id: i64,
    ) -> Result<Vec<crate::models::PortalSessionRecord>> {
        let now = now_str();
        Ok(portal_sessions::Entity::find()
            .filter(portal_sessions::Column::IdentityId.eq(identity_id))
            .filter(portal_sessions::Column::ExpiresAt.gt(now))
            .order_by_desc(portal_sessions::Column::Id)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(|r| crate::models::PortalSessionRecord {
                id: r.id,
                token_hash: r.token_hash,
                created_at: r.created_at,
                expires_at: r.expires_at,
                last_used_at: r.last_used_at,
                user_agent: r.user_agent,
                device_type: r.device_type,
                client_label: r.client_label,
            })
            .collect())
    }

    /// Delete a portal session by id only if it belongs to `identity_id`.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn delete_portal_session_by_id_for_identity(
        &self,
        id: i64,
        identity_id: i64,
    ) -> Result<bool> {
        let result = portal_sessions::Entity::delete_many()
            .filter(portal_sessions::Column::Id.eq(id))
            .filter(portal_sessions::Column::IdentityId.eq(identity_id))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(result.rows_affected > 0)
    }

    /// Count account_links referencing `account_id`.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn count_account_links_for_account(
        &self,
        plugin_key: &str,
        account_id: &str,
    ) -> Result<u64> {
        account_links::Entity::find()
            .filter(account_links::Column::PluginKey.eq(plugin_key))
            .filter(account_links::Column::AccountId.eq(account_id))
            .count(&self.db)
            .await
            .map_err(LibraryError::Orm)
    }

    /// Resolve a valid portal session to its identity.
    ///
    /// Refreshes `portal_sessions.last_used_at` and `users.last_seen_at` at most
    /// once per minute so administrator presence can tell active from idle
    /// without a write on every request. `last_seen_at` is kept after logout.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn get_portal_session_identity(
        &self,
        token_hash: &str,
    ) -> Result<Option<crate::models::PortalIdentity>> {
        let now = now_str();
        let Some(session) = portal_sessions::Entity::find()
            .filter(portal_sessions::Column::TokenHash.eq(token_hash))
            .filter(portal_sessions::Column::ExpiresAt.gt(&now))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        let identity_id = session.identity_id;
        let should_touch = match parse_dt_opt(session.last_used_at.as_deref()) {
            None => true,
            Some(at) => Utc::now() - at >= PORTAL_SESSION_LAST_USED_MIN_INTERVAL,
        };
        if should_touch {
            let mut am: portal_sessions::ActiveModel = session.into();
            am.last_used_at = Set(Some(now.clone()));
            am.update(&self.db).await.map_err(LibraryError::Orm)?;
        }
        let identity = self.get_portal_identity_by_id(identity_id).await?;
        if should_touch {
            if let Some(user_id) = identity.as_ref().and_then(|row| row.user_id) {
                Self::set_user_last_seen_on(&self.db, user_id, &now).await?;
            }
        }
        Ok(identity)
    }

    /// Writes `users.last_seen_at` for the user bound to a portal identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the identity or user update fails.
    async fn set_last_seen_for_identity_on<C: ConnectionTrait>(
        txn: &C,
        identity_id: i64,
        at: &str,
    ) -> Result<()> {
        let Some(identity) = portal_identities::Entity::find_by_id(identity_id)
            .one(txn)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(());
        };
        let Some(user_id) = identity.user_id else {
            return Ok(());
        };
        Self::set_user_last_seen_on(txn, user_id, at).await
    }

    /// Persists durable last-seen without bumping `users.updated_at`.
    ///
    /// # Errors
    ///
    /// Returns an error when the user lookup or update fails.
    async fn set_user_last_seen_on<C: ConnectionTrait>(
        txn: &C,
        user_id: i64,
        at: &str,
    ) -> Result<()> {
        let Some(model) = users::Entity::find_by_id(user_id)
            .one(txn)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(());
        };
        let mut am: users::ActiveModel = model.into();
        am.last_seen_at = Set(Some(at.to_string()));
        am.update(txn).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Create a durable operator session (hash only).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn insert_operator_session(
        &self,
        token_hash: &str,
        expires_at: chrono::DateTime<Utc>,
    ) -> Result<()> {
        self.insert_operator_session_with_client(token_hash, expires_at, None)
            .await
    }

    /// Create a durable operator session with optional client metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn insert_operator_session_with_client(
        &self,
        token_hash: &str,
        expires_at: chrono::DateTime<Utc>,
        client: Option<&crate::SessionClientInfo>,
    ) -> Result<()> {
        let now = now_str();
        let am = operator_sessions::ActiveModel {
            id: NotSet,
            token_hash: Set(token_hash.to_string()),
            expires_at: Set(expires_at.to_rfc3339()),
            created_at: Set(now.clone()),
            last_used_at: Set(Some(now)),
            elevated_from_user_id: Set(None),
            impersonating_user_id: Set(None),
            user_agent: Set(client.and_then(|c| c.user_agent.clone())),
            device_type: Set(client.map(|c| c.device_type.clone())),
            client_label: Set(client.map(|c| c.client_label.clone())),
        };
        am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Create an elevated operator session (Administrator reauth → short TTL).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn insert_elevated_operator_session(
        &self,
        token_hash: &str,
        expires_at: chrono::DateTime<Utc>,
        elevated_from_user_id: i64,
    ) -> Result<()> {
        self.insert_elevated_operator_session_with_client(
            token_hash,
            expires_at,
            elevated_from_user_id,
            None,
        )
        .await
    }

    /// Create an elevated operator session with optional client metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn insert_elevated_operator_session_with_client(
        &self,
        token_hash: &str,
        expires_at: chrono::DateTime<Utc>,
        elevated_from_user_id: i64,
        client: Option<&crate::SessionClientInfo>,
    ) -> Result<()> {
        let now = now_str();
        let am = operator_sessions::ActiveModel {
            id: NotSet,
            token_hash: Set(token_hash.to_string()),
            expires_at: Set(expires_at.to_rfc3339()),
            created_at: Set(now.clone()),
            last_used_at: Set(Some(now)),
            elevated_from_user_id: Set(Some(elevated_from_user_id)),
            impersonating_user_id: Set(None),
            user_agent: Set(client.and_then(|c| c.user_agent.clone())),
            device_type: Set(client.map(|c| c.device_type.clone())),
            client_label: Set(client.map(|c| c.client_label.clone())),
        };
        am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Look up a valid operator session (touches `last_used_at`).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn get_operator_session(
        &self,
        token_hash: &str,
    ) -> Result<Option<crate::models::OperatorSessionRecord>> {
        let now = now_str();
        let Some(session) = operator_sessions::Entity::find()
            .filter(operator_sessions::Column::TokenHash.eq(token_hash))
            .filter(operator_sessions::Column::ExpiresAt.gt(&now))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        if let Some(origin_id) = session.elevated_from_user_id {
            let origin_ok = match users::Entity::find_by_id(origin_id)
                .one(&self.db)
                .await
                .map_err(LibraryError::Orm)?
            {
                Some(u) => {
                    let rec = map_user(u);
                    matches!(rec.role, UserRole::Owner) && matches!(rec.status, UserStatus::Active)
                }
                None => false,
            };
            if !origin_ok {
                operator_sessions::Entity::delete_by_id(session.id)
                    .exec(&self.db)
                    .await
                    .map_err(LibraryError::Orm)?;
                return Ok(None);
            }
        }
        let id = session.id;
        let expires_at = session.expires_at.clone();
        let created_at = session.created_at.clone();
        let elevated_from_user_id = session.elevated_from_user_id;
        let impersonating_user_id = session.impersonating_user_id;
        let user_agent = session.user_agent.clone();
        let device_type = session.device_type.clone();
        let client_label = session.client_label.clone();
        let mut am: operator_sessions::ActiveModel = session.into();
        am.last_used_at = Set(Some(now.clone()));
        am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(Some(crate::models::OperatorSessionRecord {
            id,
            expires_at: parse_dt(&expires_at),
            created_at: parse_dt(&created_at),
            last_used_at: Some(parse_dt(&now)),
            elevated_from_user_id,
            impersonating_user_id,
            user_agent,
            device_type,
            client_label,
        }))
    }

    /// Return whether a hashed operator session is still valid (and touch last_used).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn operator_session_valid(&self, token_hash: &str) -> Result<bool> {
        Ok(self.get_operator_session(token_hash).await?.is_some())
    }

    /// Set or clear impersonation on an operator session.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn set_operator_session_impersonating(
        &self,
        token_hash: &str,
        user_id: Option<i64>,
    ) -> Result<crate::models::OperatorSessionRecord> {
        let now = now_str();
        let session = operator_sessions::Entity::find()
            .filter(operator_sessions::Column::TokenHash.eq(token_hash))
            .filter(operator_sessions::Column::ExpiresAt.gt(&now))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound("operator session".into()))?;
        let mut am: operator_sessions::ActiveModel = session.into();
        am.impersonating_user_id = Set(user_id);
        am.last_used_at = Set(Some(now));
        let model = am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(crate::models::OperatorSessionRecord {
            id: model.id,
            expires_at: parse_dt(&model.expires_at),
            created_at: parse_dt(&model.created_at),
            last_used_at: model.last_used_at.as_deref().map(parse_dt),
            elevated_from_user_id: model.elevated_from_user_id,
            impersonating_user_id: model.impersonating_user_id,
            user_agent: model.user_agent,
            device_type: model.device_type,
            client_label: model.client_label,
        })
    }

    /// Delete an operator session by token hash (logout / revoke).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn delete_operator_session(&self, token_hash: &str) -> Result<bool> {
        let result = operator_sessions::Entity::delete_many()
            .filter(operator_sessions::Column::TokenHash.eq(token_hash))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(result.rows_affected > 0)
    }

    /// Delete an operator session by row id.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn delete_operator_session_by_id(&self, id: i64) -> Result<bool> {
        let result = operator_sessions::Entity::delete_by_id(id)
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(result.rows_affected > 0)
    }

    /// Delete expired operator sessions (bounded cleanup).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn prune_expired_operator_sessions(&self) -> Result<u64> {
        let now = now_str();
        let result = operator_sessions::Entity::delete_many()
            .filter(operator_sessions::Column::ExpiresAt.lte(now))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(result.rows_affected)
    }

    /// List active operator sessions (newest first) for remote sign-out UI.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn list_operator_sessions(
        &self,
    ) -> Result<Vec<crate::models::OperatorSessionRecord>> {
        let now = now_str();
        let rows = operator_sessions::Entity::find()
            .filter(operator_sessions::Column::ExpiresAt.gt(now))
            .order_by_desc(operator_sessions::Column::Id)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(rows
            .into_iter()
            .map(|r| crate::models::OperatorSessionRecord {
                id: r.id,
                expires_at: parse_dt(&r.expires_at),
                created_at: parse_dt(&r.created_at),
                last_used_at: r.last_used_at.as_deref().map(parse_dt),
                elevated_from_user_id: r.elevated_from_user_id,
                impersonating_user_id: r.impersonating_user_id,
                user_agent: r.user_agent,
                device_type: r.device_type,
                client_label: r.client_label,
            })
            .collect())
    }

    /// First portal identity linked to a first-party user (for impersonation scoping).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn first_portal_identity_for_user(
        &self,
        user_id: i64,
    ) -> Result<Option<crate::models::PortalIdentity>> {
        Ok(portal_identities::Entity::find()
            .filter(portal_identities::Column::UserId.eq(user_id))
            .order_by_asc(portal_identities::Column::Id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_portal_identity))
    }

    /// All portal identities linked to a first-party user.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn list_portal_identities_for_user(
        &self,
        user_id: i64,
    ) -> Result<Vec<crate::models::PortalIdentity>> {
        Ok(portal_identities::Entity::find()
            .filter(portal_identities::Column::UserId.eq(user_id))
            .order_by_asc(portal_identities::Column::Id)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_portal_identity)
            .collect())
    }

    /// Link `(provider, external_user_id)` to an existing user (SSO / invite_only).
    ///
    /// Creates the identity row when missing. Refuses to steal an identity already
    /// bound to a different user.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn link_portal_identity_to_user(
        &self,
        provider: &str,
        external_user_id: &str,
        user_id: i64,
        label: Option<&str>,
    ) -> Result<crate::models::PortalIdentity> {
        if let Some(existing) = self.get_portal_identity(provider, external_user_id).await? {
            if let Some(bound) = existing.user_id {
                if bound != user_id {
                    return Err(LibraryError::Other(anyhow::anyhow!(
                        "identity already linked to another user"
                    )));
                }
            } else {
                let model = portal_identities::Entity::find_by_id(existing.id)
                    .one(&self.db)
                    .await
                    .map_err(LibraryError::Orm)?
                    .ok_or_else(|| LibraryError::NotFound("portal identity".into()))?;
                let mut am: portal_identities::ActiveModel = model.into();
                am.user_id = Set(Some(user_id));
                if label.is_some() {
                    am.label = Set(label.map(str::to_string));
                }
                let model = am.update(&self.db).await.map_err(LibraryError::Orm)?;
                return Ok(map_portal_identity(model));
            }
            return Ok(existing);
        }
        let am = portal_identities::ActiveModel {
            id: NotSet,
            provider: Set(provider.to_string()),
            external_user_id: Set(external_user_id.to_string()),
            label: Set(label.map(str::to_string)),
            user_id: Set(Some(user_id)),
            created_at: Set(now_str()),
            picture_url: Set(None),
        };
        let model = am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(map_portal_identity(model))
    }

    /// Insert a short-lived OIDC RP authorize state row.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_oidc_rp_state(
        &self,
        state_hash: &str,
        provider_id: &str,
        pkce_verifier: &str,
        nonce: &str,
        purpose: &str,
        user_id: Option<i64>,
        expires_at: chrono::DateTime<Utc>,
    ) -> Result<()> {
        let _ = self.prune_expired_oidc_rp_states().await;
        if self.count_oidc_rp_states().await? >= 256 {
            return Err(LibraryError::Other(anyhow::anyhow!(
                "too many pending OIDC login attempts"
            )));
        }
        let am = oidc_rp_states::ActiveModel {
            id: NotSet,
            state_hash: Set(state_hash.to_string()),
            provider_id: Set(provider_id.to_string()),
            pkce_verifier: Set(pkce_verifier.to_string()),
            nonce: Set(nonce.to_string()),
            purpose: Set(purpose.to_string()),
            user_id: Set(user_id),
            expires_at: Set(expires_at.to_rfc3339()),
            created_at: Set(now_str()),
        };
        am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Consume (delete) an OIDC RP state by hash if it has not expired.
    ///
    /// Find + delete run in one transaction so concurrent callbacks cannot both
    /// observe the same unused state.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn take_oidc_rp_state(
        &self,
        state_hash: &str,
    ) -> Result<Option<(String, String, String, String, Option<i64>)>> {
        if let Some(atomic) = &self.atomic {
            return atomic.take_oidc_rp_state(state_hash).await;
        }
        crate::db_atomic::take_oidc_rp_state(&self.db, state_hash).await
    }

    /// Deletes and returns one-shot OIDC RP state by hash; `None` if missing or already taken.
    #[allow(dead_code)]
    pub(crate) async fn take_oidc_rp_state_on<C: ConnectionTrait>(
        txn: &C,
        state_hash: &str,
    ) -> Result<Option<(String, String, String, String, Option<i64>)>> {
        let now = now_str();
        let Some(model) = oidc_rp_states::Entity::find()
            .filter(oidc_rp_states::Column::StateHash.eq(state_hash))
            .one(txn)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        let deleted = oidc_rp_states::Entity::delete_by_id(model.id)
            .exec(txn)
            .await
            .map_err(LibraryError::Orm)?;
        if deleted.rows_affected != 1 {
            return Ok(None);
        }
        if model.expires_at.as_str() <= now.as_str() {
            return Ok(None);
        }
        Ok(Some((
            model.provider_id,
            model.pkce_verifier,
            model.nonce,
            model.purpose,
            model.user_id,
        )))
    }

    /// Store a WebAuthn credential for a user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - First-party user who owns the passkey.
    /// * `credential_id` - Base64url credential id.
    /// * `passkey_json` - Serialized `webauthn_rs` passkey.
    /// * `name` - Label shown in Settings; empty becomes `None`.
    ///
    /// # Errors
    ///
    /// Returns an error when the insert fails (including unique credential id).
    pub async fn insert_webauthn_credential(
        &self,
        user_id: i64,
        credential_id: &str,
        passkey_json: &str,
        name: Option<&str>,
    ) -> Result<()> {
        let name = name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let am = webauthn_credentials::ActiveModel {
            id: NotSet,
            user_id: Set(user_id),
            credential_id: Set(credential_id.to_string()),
            passkey_json: Set(passkey_json.to_string()),
            name: Set(name),
            created_at: Set(now_str()),
            last_used_at: Set(None),
        };
        am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Passkeys registered to a user.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn list_webauthn_credentials(&self, user_id: i64) -> Result<Vec<StoredPasskey>> {
        Ok(webauthn_credentials::Entity::find()
            .filter(webauthn_credentials::Column::UserId.eq(user_id))
            .order_by_asc(webauthn_credentials::Column::Id)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(|m| StoredPasskey {
                id: m.id,
                credential_id: m.credential_id,
                passkey_json: m.passkey_json,
                name: m.name,
            })
            .collect())
    }

    /// Count passkeys for a user.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn count_webauthn_credentials(&self, user_id: i64) -> Result<u64> {
        webauthn_credentials::Entity::find()
            .filter(webauthn_credentials::Column::UserId.eq(user_id))
            .count(&self.db)
            .await
            .map_err(LibraryError::Orm)
    }

    /// Look up a passkey by credential id.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn get_webauthn_credential_by_cred_id(
        &self,
        credential_id: &str,
    ) -> Result<Option<(i64, i64, String)>> {
        Ok(webauthn_credentials::Entity::find()
            .filter(webauthn_credentials::Column::CredentialId.eq(credential_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(|m| (m.id, m.user_id, m.passkey_json)))
    }

    /// Replace passkey JSON after a successful assertion (counter update).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn update_webauthn_credential(&self, id: i64, passkey_json: &str) -> Result<()> {
        let model = webauthn_credentials::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound("passkey".into()))?;
        let mut am: webauthn_credentials::ActiveModel = model.into();
        am.passkey_json = Set(passkey_json.to_string());
        am.last_used_at = Set(Some(now_str()));
        am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Delete a passkey owned by `user_id`.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn delete_webauthn_credential(&self, user_id: i64, id: i64) -> Result<bool> {
        let res = webauthn_credentials::Entity::delete_many()
            .filter(webauthn_credentials::Column::Id.eq(id))
            .filter(webauthn_credentials::Column::UserId.eq(user_id))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(res.rows_affected > 0)
    }

    /// Set whether this user has a confirmed TOTP authenticator.
    ///
    /// # Arguments
    ///
    /// * `user_id` - First-party user id.
    /// * `enabled` - `true` after a successful enroll confirm; `false` on disable.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::NotFound`] when the user does not exist.
    pub async fn set_user_totp_enabled(&self, user_id: i64, enabled: bool) -> Result<()> {
        let operation_id = format!("user-totp-enabled-{}", Uuid::new_v4());
        let reply = self
            .execute_host_batch(batch::request(
                &operation_id,
                vec![
                    batch::exec(
                        "UPDATE users SET totp_enabled = ?, updated_at = ? WHERE id = ?",
                        vec![
                            batch::int(i64::from(enabled)),
                            batch::text(&now_str()),
                            batch::int(user_id),
                        ],
                    ),
                    crate::control_plane::bump_if_any(
                        LiveTopic::Users,
                        batch::query(
                            "SELECT 1 FROM users WHERE id = ?",
                            vec![batch::int(user_id)],
                            1,
                        ),
                    ),
                ],
            ))
            .await?;
        if batch::rows_affected(&reply, 0) == 0 {
            return Err(LibraryError::NotFound("user".into()));
        }
        Ok(())
    }

    /// Sets `users.totp_enabled` inside an open transaction.
    pub(crate) async fn set_user_totp_enabled_on<C: ConnectionTrait>(
        txn: &C,
        user_id: i64,
        enabled: bool,
    ) -> Result<()> {
        let model = users::Entity::find_by_id(user_id)
            .one(txn)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound("user".into()))?;
        let mut am: users::ActiveModel = model.into();
        am.totp_enabled = Set(i64::from(enabled));
        am.updated_at = Set(now_str());
        am.update(txn).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Promote a pending TOTP secret to primary and set `totp_enabled` in one transaction.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::NotFound`] when the user does not exist.
    pub async fn confirm_totp_enrollment(&self, user_id: i64, secret_b32: &str) -> Result<()> {
        let record = crate::secrets::build_sealed_record(
            secret_b32.as_bytes(),
            crate::secrets::secret_kind::TOTP,
            "local",
            crate::secrets::secret_account_type::USER,
            &user_id.to_string(),
            "primary",
        )?;
        if let Some(atomic) = &self.atomic {
            return atomic.confirm_totp_enrollment(user_id, &record).await;
        }
        crate::db_atomic::confirm_totp_enrollment(&self.db, user_id, &record).await
    }

    /// Promote a sealed TOTP secret to `primary` and set `totp_enabled` inside an open transaction.
    #[allow(dead_code)]
    pub(crate) async fn confirm_totp_enrollment_on<C: ConnectionTrait>(
        txn: &C,
        user_id: i64,
        record: &crate::secrets::EncryptedSecretRecord,
    ) -> Result<()> {
        crate::secrets::upsert_secret_on(txn, record).await?;
        crate::secrets::delete_secret_on(
            txn,
            crate::secrets::secret_kind::TOTP,
            Some("local"),
            crate::secrets::secret_account_type::USER,
            Some(&user_id.to_string()),
            "pending",
        )
        .await?;
        Self::set_user_totp_enabled_on(txn, user_id, true).await?;
        Ok(())
    }

    /// Delete TOTP secrets and clear `totp_enabled` in one transaction.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::NotFound`] when the user does not exist.
    pub async fn disable_user_totp(&self, user_id: i64) -> Result<()> {
        if let Some(atomic) = &self.atomic {
            return atomic.disable_user_totp(user_id).await;
        }
        crate::db_atomic::disable_user_totp(&self.db, user_id).await
    }

    /// Delete TOTP secrets and clear `totp_enabled` inside an open transaction.
    #[allow(dead_code)]
    pub(crate) async fn disable_user_totp_on<C: ConnectionTrait>(
        txn: &C,
        user_id: i64,
    ) -> Result<()> {
        crate::secrets::delete_secret_on(
            txn,
            crate::secrets::secret_kind::TOTP,
            Some("local"),
            crate::secrets::secret_account_type::USER,
            Some(&user_id.to_string()),
            "primary",
        )
        .await?;
        crate::secrets::delete_secret_on(
            txn,
            crate::secrets::secret_kind::TOTP,
            Some("local"),
            crate::secrets::secret_account_type::USER,
            Some(&user_id.to_string()),
            "pending",
        )
        .await?;
        Self::set_user_totp_enabled_on(txn, user_id, false).await?;
        Ok(())
    }

    /// Insert a WebAuthn ceremony state.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn insert_webauthn_challenge(
        &self,
        challenge_id: &str,
        user_id: Option<i64>,
        kind: &str,
        state_json: &str,
        expires_at: chrono::DateTime<Utc>,
    ) -> Result<()> {
        let _ = self.prune_expired_webauthn_challenges().await;
        if self.count_webauthn_challenges().await? >= 256 {
            return Err(LibraryError::Other(anyhow::anyhow!(
                "too many pending WebAuthn challenges"
            )));
        }
        let am = webauthn_challenges::ActiveModel {
            id: NotSet,
            challenge_id: Set(challenge_id.to_string()),
            user_id: Set(user_id),
            kind: Set(kind.to_string()),
            state_json: Set(state_json.to_string()),
            expires_at: Set(expires_at.to_rfc3339()),
            created_at: Set(now_str()),
        };
        am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Consume a WebAuthn ceremony by id (transactional delete-and-return).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn take_webauthn_challenge(
        &self,
        challenge_id: &str,
        kind: &str,
    ) -> Result<Option<(Option<i64>, String)>> {
        if let Some(atomic) = &self.atomic {
            return atomic.take_webauthn_challenge(challenge_id, kind).await;
        }
        crate::db_atomic::take_webauthn_challenge(&self.db, challenge_id, kind).await
    }

    /// Deletes and returns a one-shot WebAuthn challenge; `None` if missing or already taken.
    #[allow(dead_code)]
    pub(crate) async fn take_webauthn_challenge_on<C: ConnectionTrait>(
        txn: &C,
        challenge_id: &str,
        kind: &str,
    ) -> Result<Option<(Option<i64>, String)>> {
        let now = now_str();
        let Some(model) = webauthn_challenges::Entity::find()
            .filter(webauthn_challenges::Column::ChallengeId.eq(challenge_id))
            .filter(webauthn_challenges::Column::Kind.eq(kind))
            .one(txn)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        let deleted = webauthn_challenges::Entity::delete_by_id(model.id)
            .exec(txn)
            .await
            .map_err(LibraryError::Orm)?;
        if deleted.rows_affected != 1 {
            return Ok(None);
        }
        if model.expires_at.as_str() <= now.as_str() {
            return Ok(None);
        }
        Ok(Some((model.user_id, model.state_json)))
    }

    /// Drop expired OIDC RP states (bounded cleanup).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn prune_expired_oidc_rp_states(&self) -> Result<u64> {
        let now = now_str();
        let result = oidc_rp_states::Entity::delete_many()
            .filter(oidc_rp_states::Column::ExpiresAt.lte(now))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(result.rows_affected)
    }

    /// Drop expired WebAuthn challenges (bounded cleanup).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn prune_expired_webauthn_challenges(&self) -> Result<u64> {
        let now = now_str();
        let result = webauthn_challenges::Entity::delete_many()
            .filter(webauthn_challenges::Column::ExpiresAt.lte(now))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(result.rows_affected)
    }

    /// Count pending OIDC RP states (for public-begin caps).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn count_oidc_rp_states(&self) -> Result<u64> {
        oidc_rp_states::Entity::find()
            .count(&self.db)
            .await
            .map_err(LibraryError::Orm)
    }

    /// Count pending WebAuthn challenges (for public-begin caps).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn count_webauthn_challenges(&self) -> Result<u64> {
        webauthn_challenges::Entity::find()
            .count(&self.db)
            .await
            .map_err(LibraryError::Orm)
    }

    /// Created-at of a still-valid portal session (recent-auth checks).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn portal_session_created_at(
        &self,
        token_hash: &str,
    ) -> Result<Option<chrono::DateTime<Utc>>> {
        let now = now_str();
        let Some(session) = portal_sessions::Entity::find()
            .filter(portal_sessions::Column::TokenHash.eq(token_hash))
            .filter(portal_sessions::Column::ExpiresAt.gt(now))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        Ok(Some(parse_dt(&session.created_at)))
    }

    /// Overwrite `created_at` for a portal session (tests / recent-auth fixtures).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn set_portal_session_created_at(
        &self,
        token_hash: &str,
        created_at: chrono::DateTime<Utc>,
    ) -> Result<bool> {
        let Some(model) = portal_sessions::Entity::find()
            .filter(portal_sessions::Column::TokenHash.eq(token_hash))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(false);
        };
        let mut am: portal_sessions::ActiveModel = model.into();
        am.created_at = Set(created_at.to_rfc3339());
        am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(true)
    }

    /// Appends one security audit event row.
    ///
    /// # Arguments
    ///
    /// * `actor` - Actor label.
    /// * `action` - Audit action verb.
    /// * `detail_json` - `detail_json` argument for this call.
    ///
    /// # Returns
    ///
    /// `Result<crate::models::SecurityAuditEvent>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn insert_security_audit_event(
        &self,
        actor: &str,
        action: &str,
        detail_json: Option<&str>,
    ) -> Result<crate::models::SecurityAuditEvent> {
        let am = security_audit_events::ActiveModel {
            id: NotSet,
            at: Set(now_str()),
            actor: Set(actor.to_string()),
            action: Set(action.to_string()),
            detail_json: Set(detail_json.map(str::to_string)),
        };
        let model = am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(crate::models::SecurityAuditEvent {
            id: model.id,
            at: parse_dt(&model.at),
            actor: model.actor,
            action: model.action,
            detail_json: model.detail_json,
        })
    }

    /// Insert a new OIDC client (public clients may omit secret hash).
    ///
    /// Custom (operator) clients pass `plugin_id = None` and typically
    /// `enabled = true`. Plugin-owned templates pass the plugin id and start
    /// disabled.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_oidc_client(
        &self,
        client_id: &str,
        client_secret_hash: Option<&str>,
        redirect_uris: &[String],
        name: Option<&str>,
        issue_refresh_token: bool,
        allowed_scopes: &[String],
        enabled: bool,
        plugin_id: Option<&str>,
    ) -> Result<OidcClientRecord> {
        let uris = serde_json::to_string(redirect_uris)
            .map_err(|e| LibraryError::Other(anyhow::anyhow!("redirect_uris json: {e}")))?;
        let scopes = serde_json::to_string(&normalize_oidc_scopes(allowed_scopes))
            .map_err(|e| LibraryError::Other(anyhow::anyhow!("allowed_scopes json: {e}")))?;
        let plugin = plugin_id
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let am = oidc_clients::ActiveModel {
            id: NotSet,
            client_id: Set(client_id.to_string()),
            client_secret_hash: Set(client_secret_hash.map(str::to_string)),
            redirect_uris_json: Set(uris),
            name: Set(name.map(str::to_string)),
            created_at: Set(now_str()),
            issue_refresh_token: Set(i64::from(issue_refresh_token)),
            allowed_scopes_json: Set(scopes),
            enabled: Set(i64::from(enabled)),
            plugin_id: Set(plugin),
        };
        let model = am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(oidc_client_from_model(model))
    }

    /// Insert a plugin-owned OIDC client when missing; refresh redirect URIs when present.
    ///
    /// New rows start **disabled**. Existing rows owned by the **same** plugin keep
    /// `enabled`, name, scopes, and secret. Collisions with custom (operator) clients
    /// or a different plugin's `client_id` are hard errors — this never mutates
    /// an unowned row. Alias-era rows (`plugin_id = "audiobookshelf"`) are rewritten
    /// to the provenance-qualified PluginKey (`…#audiobookshelf`) on the next
    /// heartbeat from that guest; a different PluginKey with the same alias
    /// cannot take the client over.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Conflict`] when `client_id` is already owned by a
    /// custom client or another plugin.
    pub async fn upsert_plugin_oidc_client(
        &self,
        client_id: &str,
        plugin_id: &str,
        name: &str,
        redirect_uris: &[String],
        issue_refresh_token: bool,
        allowed_scopes: &[String],
    ) -> Result<OidcClientRecord> {
        if let Some(existing) = self.get_oidc_client(client_id).await? {
            let owner = existing
                .plugin_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            match owner {
                None => {
                    return Err(LibraryError::Conflict(format!(
                        "OIDC client_id `{client_id}` is a custom operator client; \
                         plugin `{plugin_id}` cannot take it over"
                    )));
                }
                Some(owner) if !oidc_plugin_owner_matches(owner, plugin_id) => {
                    return Err(LibraryError::Conflict(format!(
                        "OIDC client_id `{client_id}` is owned by plugin `{owner}`, \
                         not `{plugin_id}`"
                    )));
                }
                Some(_) => {}
            }
            let uris = serde_json::to_string(redirect_uris)
                .map_err(|e| LibraryError::Other(anyhow::anyhow!("redirect_uris json: {e}")))?;
            let mut am: oidc_clients::ActiveModel = oidc_clients::Entity::find()
                .filter(oidc_clients::Column::ClientId.eq(client_id))
                .one(&self.db)
                .await
                .map_err(LibraryError::Orm)?
                .ok_or_else(|| LibraryError::Other(anyhow::anyhow!("oidc client vanished")))?
                .into();
            am.redirect_uris_json = Set(uris);
            if existing.plugin_id.as_deref() != Some(plugin_id) {
                am.plugin_id = Set(Some(plugin_id.to_string()));
            }
            let model = am.update(&self.db).await.map_err(LibraryError::Orm)?;
            return Ok(oidc_client_from_model(model));
        }
        let scopes = if allowed_scopes.is_empty() {
            default_oidc_scopes()
        } else {
            allowed_scopes.to_vec()
        };
        self.insert_oidc_client(
            client_id,
            None,
            redirect_uris,
            Some(name),
            issue_refresh_token,
            &scopes,
            false,
            Some(plugin_id),
        )
        .await
    }

    /// Update an existing OIDC client without changing the secret unless `secret_hash` is `Some`.
    ///
    /// Pass `Some(None)` as `secret_hash` to clear the secret (public PKCE). Pass `None` to keep.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_oidc_client(
        &self,
        client_id: &str,
        redirect_uris: &[String],
        name: Option<&str>,
        issue_refresh_token: bool,
        allowed_scopes: &[String],
        enabled: bool,
        secret_hash: Option<Option<String>>,
    ) -> Result<Option<OidcClientRecord>> {
        let Some(existing) = oidc_clients::Entity::find()
            .filter(oidc_clients::Column::ClientId.eq(client_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        let uris = serde_json::to_string(redirect_uris)
            .map_err(|e| LibraryError::Other(anyhow::anyhow!("redirect_uris json: {e}")))?;
        let scopes = serde_json::to_string(&normalize_oidc_scopes(allowed_scopes))
            .map_err(|e| LibraryError::Other(anyhow::anyhow!("allowed_scopes json: {e}")))?;
        let mut am: oidc_clients::ActiveModel = existing.into();
        am.redirect_uris_json = Set(uris);
        am.name = Set(name.map(str::to_string));
        am.issue_refresh_token = Set(i64::from(issue_refresh_token));
        am.allowed_scopes_json = Set(scopes);
        am.enabled = Set(i64::from(enabled));
        if let Some(hash) = secret_hash {
            am.client_secret_hash = Set(hash);
        }
        let model = am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(Some(oidc_client_from_model(model)))
    }

    /// Replace the stored client-secret hash (or clear it).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn set_oidc_client_secret(
        &self,
        client_id: &str,
        client_secret_hash: Option<&str>,
    ) -> Result<Option<OidcClientRecord>> {
        let Some(existing) = oidc_clients::Entity::find()
            .filter(oidc_clients::Column::ClientId.eq(client_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        let mut am: oidc_clients::ActiveModel = existing.into();
        am.client_secret_hash = Set(client_secret_hash.map(str::to_string));
        let model = am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(Some(oidc_client_from_model(model)))
    }

    /// Delete a registered OIDC client by `client_id`.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn delete_oidc_client(&self, client_id: &str) -> Result<bool> {
        let res = oidc_clients::Entity::delete_many()
            .filter(oidc_clients::Column::ClientId.eq(client_id))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(res.rows_affected > 0)
    }

    /// List registered OIDC clients, oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn list_oidc_clients(&self) -> Result<Vec<OidcClientRecord>> {
        let rows = oidc_clients::Entity::find()
            .order_by_asc(oidc_clients::Column::Id)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(rows.into_iter().map(oidc_client_from_model).collect())
    }

    /// Loads a registered OIDC client by `client_id`, if present.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn get_oidc_client(&self, client_id: &str) -> Result<Option<OidcClientRecord>> {
        let Some(model) = oidc_clients::Entity::find()
            .filter(oidc_clients::Column::ClientId.eq(client_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        Ok(Some(oidc_client_from_model(model)))
    }

    /// Persists a hashed OIDC authorization code with PKCE metadata.
    ///
    /// # Arguments
    ///
    /// * `code_hash` - SHA-256 hex digest of the authorization code.
    /// * `client_id` - OIDC client_id.
    /// * `user_id` - First-party user id.
    /// * `redirect_uri` - OAuth redirect URI.
    /// * `code_challenge` - PKCE code_challenge.
    /// * `code_challenge_method` - PKCE method (`S256` or `plain`).
    /// * `scope` - OAuth scope string.
    /// * `expires_at` - Expiry timestamp.
    ///
    /// # Returns
    ///
    /// `Result<()>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_oidc_auth_code(
        &self,
        code_hash: &str,
        client_id: &str,
        user_id: i64,
        redirect_uri: &str,
        code_challenge: &str,
        code_challenge_method: &str,
        scope: &str,
        expires_at: chrono::DateTime<Utc>,
    ) -> Result<()> {
        let am = oidc_auth_codes::ActiveModel {
            id: NotSet,
            code_hash: Set(code_hash.to_string()),
            client_id: Set(client_id.to_string()),
            user_id: Set(user_id),
            redirect_uri: Set(redirect_uri.to_string()),
            code_challenge: Set(code_challenge.to_string()),
            code_challenge_method: Set(code_challenge_method.to_string()),
            scope: Set(scope.to_string()),
            expires_at: Set(expires_at.to_rfc3339()),
            consumed_at: Set(None),
            created_at: Set(now_str()),
        };
        am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Reads an unconsumed, unexpired auth code without consuming it; returns
    /// (client_id, user_id, redirect_uri, challenge, method, scope). For a
    /// caller that records what the code grants before it consumes the code,
    /// so a failed write leaves the code usable for a retry.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn peek_oidc_auth_code(
        &self,
        code_hash: &str,
    ) -> Result<Option<(String, i64, String, String, String, String)>> {
        let row = oidc_auth_codes::Entity::find()
            .filter(oidc_auth_codes::Column::CodeHash.eq(code_hash))
            .filter(oidc_auth_codes::Column::ConsumedAt.is_null())
            .filter(oidc_auth_codes::Column::ExpiresAt.gt(now_str()))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(row.map(|row| {
            (
                row.client_id,
                row.user_id,
                row.redirect_uri,
                row.code_challenge,
                row.code_challenge_method,
                row.scope,
            )
        }))
    }

    /// Atomically consume an auth code; returns (client_id, user_id, redirect_uri, challenge, method, scope).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn consume_oidc_auth_code(
        &self,
        code_hash: &str,
    ) -> Result<Option<(String, i64, String, String, String, String)>> {
        use sea_orm::sea_query::Expr;

        let now = now_str();
        let Some(row) = oidc_auth_codes::Entity::find()
            .filter(oidc_auth_codes::Column::CodeHash.eq(code_hash))
            .filter(oidc_auth_codes::Column::ConsumedAt.is_null())
            .filter(oidc_auth_codes::Column::ExpiresAt.gt(&now))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        let result = oidc_auth_codes::Entity::update_many()
            .col_expr(oidc_auth_codes::Column::ConsumedAt, Expr::value(now))
            .filter(oidc_auth_codes::Column::Id.eq(row.id))
            .filter(oidc_auth_codes::Column::ConsumedAt.is_null())
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if result.rows_affected != 1 {
            return Ok(None);
        }
        Ok(Some((
            row.client_id,
            row.user_id,
            row.redirect_uri,
            row.code_challenge,
            row.code_challenge_method,
            row.scope,
        )))
    }

    /// Persists a hashed OIDC refresh token.
    ///
    /// # Arguments
    ///
    /// * `token_hash` - SHA-256 hex digest of the opaque token.
    /// * `client_id` - OIDC client_id.
    /// * `user_id` - First-party user id.
    /// * `scope` - OAuth scope string.
    /// * `expires_at` - Expiry timestamp.
    ///
    /// # Returns
    ///
    /// `Result<()>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn insert_oidc_refresh_token(
        &self,
        token_hash: &str,
        client_id: &str,
        user_id: i64,
        scope: &str,
        expires_at: chrono::DateTime<Utc>,
    ) -> Result<()> {
        let am = oidc_refresh_tokens::ActiveModel {
            id: NotSet,
            token_hash: Set(token_hash.to_string()),
            client_id: Set(client_id.to_string()),
            user_id: Set(user_id),
            scope: Set(scope.to_string()),
            expires_at: Set(expires_at.to_rfc3339()),
            revoked_at: Set(None),
            created_at: Set(now_str()),
        };
        am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Loads a refresh token row by hash, if present.
    ///
    /// # Arguments
    ///
    /// * `token_hash` - SHA-256 hex digest of the opaque token.
    ///
    /// # Returns
    ///
    /// `Result<Option<(String, i64, String)>>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn get_oidc_refresh_token(
        &self,
        token_hash: &str,
    ) -> Result<Option<(String, i64, String)>> {
        let now = now_str();
        let Some(row) = oidc_refresh_tokens::Entity::find()
            .filter(oidc_refresh_tokens::Column::TokenHash.eq(token_hash))
            .filter(oidc_refresh_tokens::Column::RevokedAt.is_null())
            .filter(oidc_refresh_tokens::Column::ExpiresAt.gt(now))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        Ok(Some((row.client_id, row.user_id, row.scope)))
    }

    /// Marks a refresh token revoked; returns whether a row was updated.
    ///
    /// # Arguments
    ///
    /// * `token_hash` - SHA-256 hex digest of the opaque token.
    ///
    /// # Returns
    ///
    /// `Result<bool>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn revoke_oidc_refresh_token(&self, token_hash: &str) -> Result<bool> {
        use sea_orm::sea_query::Expr;
        let result = oidc_refresh_tokens::Entity::update_many()
            .col_expr(
                oidc_refresh_tokens::Column::RevokedAt,
                Expr::value(now_str()),
            )
            .filter(oidc_refresh_tokens::Column::TokenHash.eq(token_hash))
            .filter(oidc_refresh_tokens::Column::RevokedAt.is_null())
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(result.rows_affected > 0)
    }

    /// List recent security audit events (newest first).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn list_security_audit_events(
        &self,
        limit: u64,
    ) -> Result<Vec<crate::models::SecurityAuditEvent>> {
        Ok(security_audit_events::Entity::find()
            .order_by_desc(security_audit_events::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(|m| crate::models::SecurityAuditEvent {
                id: m.id,
                at: parse_dt(&m.at),
                actor: m.actor,
                action: m.action,
                detail_json: m.detail_json,
            })
            .collect())
    }

    /// Link a bookstore account to a portal identity.
    ///
    /// Account links are exclusive: one `(plugin_key, account_id)` may belong
    /// to only one identity (`idx_account_links_account_exclusive`).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn link_account(
        &self,
        identity_id: i64,
        plugin_key: &str,
        account_id: &str,
        source: &str,
    ) -> Result<crate::models::AccountLinkRecord> {
        let existing = account_links::Entity::find()
            .filter(account_links::Column::IdentityId.eq(identity_id))
            .filter(account_links::Column::PluginKey.eq(plugin_key))
            .filter(account_links::Column::AccountId.eq(account_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if let Some(model) = existing {
            return Ok(map_account_link(model));
        }
        if let Some(other) = account_links::Entity::find()
            .filter(account_links::Column::PluginKey.eq(plugin_key))
            .filter(account_links::Column::AccountId.eq(account_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        {
            if other.identity_id != identity_id {
                return Err(LibraryError::Other(anyhow::anyhow!(
                    "account `{account_id}` is already linked to another user"
                )));
            }
        }
        let am = account_links::ActiveModel {
            id: NotSet,
            identity_id: Set(identity_id),
            plugin_key: Set(plugin_key.to_string()),
            account_id: Set(account_id.to_string()),
            source: Set(source.to_string()),
            created_at: Set(now_str()),
        };
        let model = am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(map_account_link(model))
    }

    /// Lists store accounts linked to a portal identity.
    ///
    /// # Arguments
    ///
    /// * `identity_id` - Portal identity id.
    ///
    /// # Returns
    ///
    /// `Result<Vec<crate::models::AccountLinkRecord>>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn list_account_links(
        &self,
        identity_id: i64,
    ) -> Result<Vec<crate::models::AccountLinkRecord>> {
        Ok(account_links::Entity::find()
            .filter(account_links::Column::IdentityId.eq(identity_id))
            .order_by_asc(account_links::Column::Id)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_account_link)
            .collect())
    }

    /// Remove an account link row (does not delete the account).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn unlink_account(
        &self,
        identity_id: i64,
        plugin_key: &str,
        account_id: &str,
    ) -> Result<()> {
        let res = account_links::Entity::delete_many()
            .filter(account_links::Column::IdentityId.eq(identity_id))
            .filter(account_links::Column::PluginKey.eq(plugin_key))
            .filter(account_links::Column::AccountId.eq(account_id))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if res.rows_affected == 0 {
            return Err(LibraryError::NotFound(account_id.into()));
        }
        Ok(())
    }

    /// Upsert a book from a library sync and resolve the listing it
    /// describes into works, expressions and editions.
    ///
    /// The listing is derived from the flat row ([`listing_from_book`]);
    /// a scan that has the plugin's structured record uses
    /// [`Self::upsert_scanned_title`] instead. A row that fails listing
    /// validation, or whose listing cannot be resolved, is still stored,
    /// and the listing is skipped with a warning.
    ///
    /// # Errors
    ///
    /// Returns an error when the flat row cannot be read or written.
    pub async fn upsert_book(&self, book: &NewBook) -> Result<BookRecord> {
        let record = self.upsert_book_row(book).await?;
        match listing_from_book(book) {
            Ok(listing) => {
                self.record_listing_after_row(&listing, &holding_from_book(book), &record)
                    .await;
            }
            Err(err) => tracing::warn!(
                product_id = %book.product_id,
                plugin_key = %book.plugin_key,
                error = %err,
                "library title not resolved into the library model"
            ),
        }
        Ok(record)
    }

    /// Upsert a scanned title: the flat row, and the listing its plugin's
    /// structured record describes (or, without one, the listing the flat
    /// row describes).
    ///
    /// The listing is filed under the book's plugin, whatever plugin key it
    /// carries.
    ///
    /// # Errors
    ///
    /// Returns an error when the flat row cannot be read or written; a
    /// listing that cannot be resolved is logged, not returned.
    pub async fn upsert_scanned_title(&self, title: &ScannedTitle) -> Result<BookRecord> {
        let Some(listing) = &title.listing else {
            return self.upsert_book(&title.book).await;
        };
        let record = self.upsert_book_row(&title.book).await?;
        let mut listing = listing.clone();
        listing.key.plugin_key.clone_from(&title.book.plugin_key);
        self.record_listing_after_row(&listing, &holding_from_book(&title.book), &record)
            .await;
        Ok(record)
    }

    /// Upsert a page of scanned titles: the flat rows as
    /// [`Self::upsert_scanned_title`] writes each one, read and written per
    /// page in batches sized from the database plugin's limits, then every
    /// title's listing resolved as one page (`record_listing_page`),
    /// so titles the library already holds as the store describes them cost
    /// no reads of their own.
    ///
    /// # Returns
    ///
    /// The stored flat rows, in the order of `titles`.
    ///
    /// # Errors
    ///
    /// Returns an error when the flat rows cannot be read or written; the
    /// batches before the failure stay written and no listing is resolved. A listing
    /// that cannot be resolved is logged, not returned.
    pub async fn upsert_scanned_titles(&self, titles: &[ScannedTitle]) -> Result<Vec<BookRecord>> {
        let books: Vec<&NewBook> = titles.iter().map(|title| &title.book).collect();
        let records = self.upsert_book_rows(&books).await?;
        let mut listings = Vec::with_capacity(titles.len());
        for (title, record) in titles.iter().zip(&records) {
            let listing = match &title.listing {
                Some(listing) => {
                    let mut listing = listing.clone();
                    listing.key.plugin_key.clone_from(&title.book.plugin_key);
                    Some(listing)
                }
                None => match listing_from_book(&title.book) {
                    Ok(listing) => Some(listing),
                    Err(err) => {
                        tracing::warn!(
                            product_id = %title.book.product_id,
                            plugin_key = %title.book.plugin_key,
                            error = %err,
                            "library title not resolved into the library model"
                        );
                        None
                    }
                },
            };
            if let Some(listing) = listing {
                listings.push((
                    listing,
                    holding_from_book(&title.book),
                    WorkEnrichment::of_book(record),
                ));
            }
        }
        self.record_listing_page(&listings).await;
        Ok(records)
    }

    /// Records a title's listing once its flat row is stored, logging a
    /// failure rather than returning it.
    ///
    /// The flat row is already committed and is what acquire and the UI
    /// read until the library model replaces it, so one title that cannot
    /// be resolved (other writers kept racing it, or a stored row is
    /// inconsistent) must not stop the rest of a scan. The next scan
    /// resolves the listing again.
    ///
    /// The work the listing reaches also takes the row's description,
    /// subjects, categories and cover where it has none yet.
    async fn record_listing_after_row(
        &self,
        listing: &bookclerk_resolve::Listing,
        holding: &bookclerk_resolve::HoldingDraft,
        record: &BookRecord,
    ) {
        let result = match self.record_listing(listing, holding).await {
            Ok(plan) => {
                self.fill_work_from_book(&plan.listing.edition_id, &WorkEnrichment::of_book(record))
                    .await
            }
            Err(err) => Err(err),
        };
        if let Err(err) = result {
            tracing::warn!(
                product_id = %listing.key.product_id,
                plugin_key = %listing.key.plugin_key,
                error = %err,
                "library title stored but not resolved into the library model; the next scan retries"
            );
        }
    }

    /// Insert on first sight; on `(plugin_key, account_id, product_id)`
    /// conflict, merge fields with Audible-precedence rules
    /// ([`merged_book_row`]). Expressed as load-then-merge over the `books`
    /// entity so the precedence logic is explicit and portable.
    async fn upsert_book_row(&self, book: &NewBook) -> Result<BookRecord> {
        let book = decoded_book(book);
        require_plugin_key(&book)?;
        let now = now_str();
        let existing = books::Entity::find()
            .filter(books::Column::PluginKey.eq(book.plugin_key.as_str()))
            .filter(books::Column::AccountId.eq(book.account_id.as_str()))
            .filter(books::Column::ProductId.eq(book.product_id.as_str()))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let model = if let Some(existing) = existing {
            let merged = merged_book_row(&existing, &book, &now);
            self.update_book_bumped(scan_update(existing, merged))
                .await?
        } else {
            let mut am = books::ActiveModel::from(new_book_row(&book, &now)).reset_all();
            am.id = NotSet;
            let mut written = am.clone();
            let id = self
                .insert_bumped(books::Entity::insert(am), LiveTopic::Library)
                .await?;
            // Every column was set, so the new row is known without
            // reading it back.
            written.id = Set(id);
            match sea_orm::TryIntoModel::try_into_model(written) {
                Ok(model) => model,
                Err(_) => self.book_model_by_id(id).await?,
            }
        };
        map_book(model)
    }

    /// Upserts a page of flat book rows as [`Self::upsert_book_row`] stores
    /// each one, with one keyed read of the rows already stored and the
    /// writes sent as atomic batches sized from the database plugin's
    /// limits.
    ///
    /// A title listed twice merges as row-by-row upserts would and is
    /// written once, as its final row; both entries return that row. A
    /// row another writer inserted between the read and the write (the
    /// insert is ignored) is merged again on its own, so no title is lost.
    ///
    /// # Returns
    ///
    /// The stored rows, in the order of `books`.
    ///
    /// # Errors
    ///
    /// Returns an error when a book has no plugin key or a read or batch
    /// fails. Batches sent before the failure stay written; each one is a
    /// set of idempotent upserts, so the next scan writes the page again.
    async fn upsert_book_rows(&self, books: &[&NewBook]) -> Result<Vec<BookRecord>> {
        let books: Vec<Cow<'_, NewBook>> = books.iter().map(|book| decoded_book(book)).collect();
        for book in &books {
            require_plugin_key(book)?;
        }
        let stored = self.stored_book_rows(&books).await?;
        self.write_book_rows(&books, stored).await
    }

    /// The write half of [`Self::upsert_book_rows`]: merges `books` over
    /// `stored` (the rows read for them), writes each title once, and reads
    /// the page back.
    ///
    /// An insert the database ignored (its key or uuid was taken after
    /// `stored` was read) is told apart by the batch reply's affected-row
    /// count, not by comparing rows, and that title is merged again on its
    /// own against the row that won.
    ///
    /// # Errors
    ///
    /// Returns an error when a batch or the re-read fails, or a title's
    /// own upsert does.
    async fn write_book_rows(
        &self,
        books: &[Cow<'_, NewBook>],
        mut rows: HashMap<BookKey, books::Model>,
    ) -> Result<Vec<BookRecord>> {
        let now = now_str();
        let mut inserts: HashSet<BookKey> = HashSet::new();
        // Fold every listing of a title into its final row first, so each
        // title is written once: an update never builds on an insert that
        // another writer's row may have turned away.
        let mut order: Vec<BookKey> = Vec::new();
        let mut seen: HashSet<BookKey> = HashSet::new();
        for book in books {
            let key = BookKey::of(book);
            let row = match rows.get(&key) {
                Some(existing) => merged_book_row(existing, book, &now),
                None => {
                    inserts.insert(key.clone());
                    new_book_row(book, &now)
                }
            };
            if seen.insert(key.clone()) {
                order.push(key.clone());
            }
            rows.insert(key, row);
        }
        // Each batch bumps the library topic with the rows it writes.
        let mut batches = WriteBatches::new(self.db_capabilities(), "library title")
            .with_trailer(crate::control_plane::bump(LiveTopic::Library))?;
        // The titles of the statements queued in the open batch, in order.
        let mut queued: Vec<BookKey> = Vec::new();
        let mut ignored: HashSet<BookKey> = HashSet::new();
        for key in order {
            let Some(row) = rows.get(&key) else { continue };
            let statement = if inserts.contains(&key) {
                insert_book_statement(row)
            } else {
                update_book_statement(row)
            };
            if let Some(full) = batches.push(statement)? {
                let sent = std::mem::take(&mut queued);
                ignored.extend(self.run_book_batch(full, sent, &inserts).await?);
            }
            queued.push(key);
        }
        if let Some(last) = batches.finish() {
            ignored.extend(self.run_book_batch(last, queued, &inserts).await?);
        }
        let written = self.stored_book_rows(books).await?;
        let mut records = Vec::with_capacity(books.len());
        for book in books {
            let key = BookKey::of(book);
            match written.get(&key) {
                Some(row) if !ignored.contains(&key) => records.push(map_book(row.clone())?),
                // Another writer's insert won, or the insert was refused
                // (a uuid the plugin sent is another row's): merge alone.
                _ => records.push(self.upsert_book_row(book).await?),
            }
        }
        Ok(records)
    }

    /// The stored flat rows of `books`, by key, read in chunks of ids the
    /// backend accepts.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    async fn stored_book_rows(
        &self,
        books: &[Cow<'_, NewBook>],
    ) -> Result<HashMap<BookKey, books::Model>> {
        let mut by_account: BTreeMap<(&str, &str), BTreeSet<String>> = BTreeMap::new();
        for book in books {
            by_account
                .entry((book.plugin_key.as_str(), book.account_id.as_str()))
                .or_default()
                .insert(book.product_id.clone());
        }
        let mut rows = HashMap::new();
        for ((plugin_key, account_id), ids) in by_account {
            let ids: Vec<String> = ids.into_iter().collect();
            let found = library_model::find_chunked::<books::Entity, _>(self, &ids, |part| {
                books::Entity::find()
                    .filter(books::Column::PluginKey.eq(plugin_key))
                    .filter(books::Column::AccountId.eq(account_id))
                    .filter(books::Column::ProductId.is_in(part.to_vec()))
            })
            .await?;
            for row in found {
                rows.insert(
                    BookKey {
                        plugin_key: row.plugin_key.clone(),
                        account_id: row.account_id.clone(),
                        product_id: row.product_id.clone(),
                    },
                    row,
                );
            }
        }
        Ok(rows)
    }

    /// Sends one chunk of flat book-row writes as one atomic host batch.
    ///
    /// `keys` names the title of each statement before the batch's trailer,
    /// in order; `inserts` holds the titles written as inserts.
    ///
    /// # Returns
    ///
    /// The titles whose insert the database ignored (no row affected).
    ///
    /// # Errors
    ///
    /// Returns the database error; nothing in the chunk is kept then.
    async fn run_book_batch(
        &self,
        statements: Vec<bookclerk_plugin_abi::TypedDbStatement>,
        keys: Vec<BookKey>,
        inserts: &HashSet<BookKey>,
    ) -> Result<Vec<BookKey>> {
        let operation_id = format!("scanned-books-{}", Uuid::new_v4());
        let reply = self
            .execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        Ok(keys
            .into_iter()
            .zip(&reply.statements)
            .filter(|(key, result)| inserts.contains(key) && result.rows_affected == 0)
            .map(|(key, _)| key)
            .collect())
    }

    /// Look up a book by its public `uuid`.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn get_book_by_uuid(&self, uuid: &str) -> Result<Option<BookRecord>> {
        books::Entity::find()
            .filter(books::Column::Uuid.eq(uuid))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_book)
            .transpose()
    }

    /// Look up a book by uuid, product_id, asin, or isbn for the given account.
    ///
    /// Prefer exact uuid / product_id matches, then asin, then isbn. When two
    /// plugins own the same account id and both match at the same priority,
    /// the lookup fails closed; pass the plugin key through
    /// [`Self::get_book_in`] instead.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Conflict`] when the match is ambiguous.
    pub async fn get_book(&self, title_id: &str, account_id: &str) -> Result<Option<BookRecord>> {
        self.get_book_in(None, title_id, account_id).await
    }

    /// [`Self::get_book`] narrowed to one plugin key when `plugin_key` is set.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Conflict`] when the match is ambiguous.
    pub async fn get_book_in(
        &self,
        plugin_key: Option<&str>,
        title_id: &str,
        account_id: &str,
    ) -> Result<Option<BookRecord>> {
        self.resolve_book_in(plugin_key, title_id, account_id)
            .await?
            .map(map_book)
            .transpose()
    }

    /// All ownership rows sharing an ISBN (cross-account / cross-store enrichment).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn find_books_by_isbn(&self, isbn: &str) -> Result<Vec<BookRecord>> {
        books::Entity::find()
            .filter(books::Column::Isbn.eq(isbn))
            .order_by_asc(books::Column::Source)
            .order_by_asc(books::Column::AccountId)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_book)
            .collect()
    }

    /// Lists books, optionally filtered to one `account_id`.
    ///
    /// # Arguments
    ///
    /// * `account_id` - Store account id.
    ///
    /// # Returns
    ///
    /// `Result<Vec<BookRecord>>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn list_books(&self, account_id: Option<&str>) -> Result<Vec<BookRecord>> {
        let mut query = books::Entity::find().order_by_asc(books::Column::Title);
        if let Some(account_id) = account_id {
            query = query.filter(books::Column::AccountId.eq(account_id));
        }
        query
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_book)
            .collect()
    }

    /// Count book rows (SQL `COUNT`, not a full fetch).
    ///
    /// A peer writer on the same SQLite file (TRUNCATE journal) can make this
    /// read return `SQLITE_BUSY`. The read is repeated until the lock clears
    /// or the attempt budget is spent. `account_id` is unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails for a reason other
    /// than file-lock contention, and [`LibraryError::Unavailable`] when the
    /// lock is still held after the retries.
    pub async fn count_books(&self, account_id: Option<&str>) -> Result<i64> {
        lock_retry::retry_read_lock(|| async {
            let mut query = books::Entity::find();
            if let Some(account_id) = account_id {
                query = query.filter(books::Column::AccountId.eq(account_id));
            }
            let count = query
                .count(&self.db)
                .await
                .map_err(LibraryError::from_db_err)?;
            Ok(count as i64)
        })
        .await
    }

    /// Resolve `title_id` to the stored public book model, or error if missing.
    async fn resolve_book(&self, title_id: &str, account_id: &str) -> Result<books::Model> {
        self.resolve_book_in(None, title_id, account_id)
            .await?
            .ok_or_else(|| LibraryError::NotFound(title_id.into()))
    }

    /// Best match for `title_id` on `account_id`, optionally within one plugin.
    ///
    /// Two rows at the same match priority under different plugin keys are
    /// ambiguous and fail closed rather than picking one.
    async fn resolve_book_in(
        &self,
        plugin_key: Option<&str>,
        title_id: &str,
        account_id: &str,
    ) -> Result<Option<books::Model>> {
        let mut query = books::Entity::find()
            .filter(books::Column::AccountId.eq(account_id))
            .filter(
                Condition::any()
                    .add(books::Column::Uuid.eq(title_id))
                    .add(books::Column::ProductId.eq(title_id))
                    .add(books::Column::Asin.eq(title_id))
                    .add(books::Column::Isbn.eq(title_id)),
            );
        if let Some(key) = plugin_key {
            query = query.filter(books::Column::PluginKey.eq(key));
        }
        let mut matches = query.all(&self.db).await.map_err(LibraryError::Orm)?;
        matches.sort_by(|a, b| {
            match_priority(a, title_id)
                .cmp(&match_priority(b, title_id))
                .then_with(|| a.plugin_key.cmp(&b.plugin_key))
        });
        let mut iter = matches.into_iter();
        let Some(best) = iter.next() else {
            return Ok(None);
        };
        if let Some(next) = iter.next() {
            if match_priority(&next, title_id) == match_priority(&best, title_id)
                && next.plugin_key != best.plugin_key
            {
                return Err(LibraryError::Conflict(format!(
                    "title `{title_id}` on account `{account_id}` is owned by both `{}` and `{}`; pass the plugin key",
                    best.plugin_key, next.plugin_key
                )));
            }
        }
        Ok(Some(best))
    }

    /// Update user-defined fields (tags, ratings, finished) without touching scan metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn update_user_fields(
        &self,
        title_id: &str,
        account_id: &str,
        fields: &UserBookFields,
    ) -> Result<()> {
        let model = self.resolve_book(title_id, account_id).await?;
        let mut am: books::ActiveModel = model.into();
        if let Some(tags) = &fields.tags {
            am.tags = Set(Some(tags.clone()));
        }
        if let Some(v) = fields.rating_overall {
            am.rating_overall = Set(Some(f64::from(v)));
        }
        if let Some(v) = fields.rating_performance {
            am.rating_performance = Set(Some(f64::from(v)));
        }
        if let Some(v) = fields.rating_story {
            am.rating_story = Set(Some(f64::from(v)));
        }
        if let Some(v) = fields.is_finished {
            am.is_finished = Set(i64::from(v));
        }
        am.updated_at = Set(now_str());
        let book = self.update_book_bumped(am).await?;
        // Each person linked to the account keeps the state on the work, so
        // members sharing the account do not overwrite one another's.
        let users = self
            .users_linked_to_account(&book.plugin_key, &book.account_id)
            .await?;
        if !users.is_empty() {
            if let Some(work_id) = self.work_id_for_book(&book.uuid).await? {
                for user_id in users {
                    self.set_user_title_state(user_id, &work_id, fields).await?;
                }
            }
        }
        Ok(())
    }

    /// Updates companion PDF acquire status and optional storage key.
    ///
    /// # Arguments
    ///
    /// * `title_id` - Product id / ASIN / ISBN used as the title key.
    /// * `account_id` - Store account id.
    /// * `status` - Acquire or request status value.
    /// * `pdf_storage_key` - Object-storage key for the companion PDF.
    ///
    /// # Returns
    ///
    /// `Result<()>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn set_pdf_status(
        &self,
        title_id: &str,
        account_id: &str,
        status: AcquireStatus,
        pdf_storage_key: Option<&str>,
    ) -> Result<()> {
        let model = self.resolve_book(title_id, account_id).await?;
        let mut am: books::ActiveModel = model.into();
        am.pdf_status = Set(status.as_str().to_string());
        am.pdf_storage_key = Set(pdf_storage_key.map(str::to_string));
        am.updated_at = Set(now_str());
        self.update_book_bumped(am).await?;
        Ok(())
    }

    /// Returns whether (`title_id`, `account_id`) is on the ignore list.
    ///
    /// # Arguments
    ///
    /// * `title_id` - Product id / ASIN / ISBN used as the title key.
    /// * `account_id` - Store account id.
    ///
    /// # Returns
    ///
    /// `Result<bool>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn is_ignored(&self, title_id: &str, account_id: &str) -> Result<bool> {
        let Some((plugin_key, product_id)) = self.ignore_key(title_id, account_id).await? else {
            return Ok(false);
        };
        Ok(ignored_titles::Entity::find()
            .filter(ignored_titles::Column::PluginKey.eq(plugin_key))
            .filter(ignored_titles::Column::AccountId.eq(account_id))
            .filter(ignored_titles::Column::ProductId.eq(product_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .is_some())
    }

    /// Adds or removes a title from the per-account ignore list.
    ///
    /// # Arguments
    ///
    /// * `title_id` - Product id / ASIN / ISBN used as the title key.
    /// * `account_id` - Store account id.
    /// * `ignored` - When true, add to the ignore list; when false, remove.
    /// * `reason` - Optional operator note for why the title is ignored.
    ///
    /// # Returns
    ///
    /// `Result<()>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn set_ignored(
        &self,
        title_id: &str,
        account_id: &str,
        ignored: bool,
        reason: Option<&str>,
    ) -> Result<()> {
        let Some((plugin_key, product_id)) = self.ignore_key(title_id, account_id).await? else {
            return Err(LibraryError::NotFound(format!(
                "title `{title_id}` is not in the library for account `{account_id}`"
            )));
        };
        if ignored {
            let existing = ignored_titles::Entity::find()
                .filter(ignored_titles::Column::PluginKey.eq(plugin_key.as_str()))
                .filter(ignored_titles::Column::AccountId.eq(account_id))
                .filter(ignored_titles::Column::ProductId.eq(product_id.as_str()))
                .one(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            if let Some(model) = existing {
                let mut am: ignored_titles::ActiveModel = model.into();
                am.reason = Set(reason.map(str::to_string));
                am.update(&self.db).await.map_err(LibraryError::Orm)?;
            } else {
                let am = ignored_titles::ActiveModel {
                    plugin_key: Set(plugin_key),
                    account_id: Set(account_id.to_string()),
                    product_id: Set(product_id),
                    reason: Set(reason.map(str::to_string)),
                    created_at: Set(now_str()),
                };
                am.insert(&self.db).await.map_err(LibraryError::Orm)?;
            }
        } else {
            ignored_titles::Entity::delete_many()
                .filter(ignored_titles::Column::PluginKey.eq(plugin_key))
                .filter(ignored_titles::Column::AccountId.eq(account_id))
                .filter(ignored_titles::Column::ProductId.eq(product_id))
                .exec(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
        }
        Ok(())
    }

    /// Resolve the `(plugin_key, product_id)` ignore key for a title. An
    /// unknown title has no owning plugin and cannot be ignored.
    async fn ignore_key(
        &self,
        title_id: &str,
        account_id: &str,
    ) -> Result<Option<(String, String)>> {
        Ok(self
            .get_book(title_id, account_id)
            .await?
            .map(|b| (b.plugin_key, b.product_id)))
    }

    /// Updates acquire status, storage key, and optional error message.
    ///
    /// # Arguments
    ///
    /// * `title_id` - Product id / ASIN / ISBN used as the title key.
    /// * `account_id` - Store account id.
    /// * `status` - Acquire or request status value.
    /// * `storage_key` - Object-storage key for the primary audio artifact.
    /// * `error_message` - Optional failure message to persist.
    ///
    /// # Returns
    ///
    /// `Result<()>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn set_acquire_status(
        &self,
        title_id: &str,
        account_id: &str,
        status: AcquireStatus,
        storage_key: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<()> {
        let model = self.resolve_book(title_id, account_id).await?;
        if let Some(atomic) = &self.atomic {
            let event = if status == AcquireStatus::Acquired {
                Some(event_outbox::prepare_publish_domain_event(
                    event_outbox::book_acquired_spec(&model, storage_key),
                )?)
            } else {
                None
            };
            return atomic
                .set_acquire_status(&model.uuid, status, storage_key, error_message, event)
                .await;
        }
        // In-process fallback for a store opened without an atomic backend
        // (local tools and tests). Hosts attach the database plugin's named
        // atomic operation above; an external adapter refuses this `begin()`.
        #[allow(clippy::disallowed_methods)]
        let txn = self.db.begin().await.map_err(LibraryError::Orm)?;
        match event_outbox::set_acquire_status_on(&txn, model, status, storage_key, error_message)
            .await
        {
            Ok(()) => {
                txn.commit().await.map_err(LibraryError::Orm)?;
                Ok(())
            }
            Err(err) => {
                let _ = txn.rollback().await;
                Err(err)
            }
        }
    }

    /// Bulk-update acquire status (classic `set-status --force`).
    ///
    /// When `asins` is non-empty, matches against uuid, product_id, isbn, or asin.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn bulk_set_acquire_status(
        &self,
        account: Option<&str>,
        asins: &[String],
        status: AcquireStatus,
    ) -> Result<u32> {
        let books = self.list_books(account).await?;
        let mut updated = 0u32;
        for book in books {
            if !asins.is_empty()
                && !asins.iter().any(|a| {
                    a.eq_ignore_ascii_case(&book.uuid)
                        || a.eq_ignore_ascii_case(&book.product_id)
                        || book
                            .isbn
                            .as_ref()
                            .is_some_and(|isbn| a.eq_ignore_ascii_case(isbn))
                        || book
                            .asin
                            .as_ref()
                            .is_some_and(|asin| a.eq_ignore_ascii_case(asin))
                })
            {
                continue;
            }
            self.set_acquire_status(
                &book.uuid,
                &book.account_id,
                status,
                book.storage_key.as_deref(),
                None,
            )
            .await?;
            updated += 1;
        }
        Ok(updated)
    }

    /// List saved quick-filter expressions.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn list_saved_filters(&self) -> Result<Vec<SavedFilterRecord>> {
        Ok(saved_filters::Entity::find()
            .order_by_asc(saved_filters::Column::Name)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_saved_filter)
            .collect())
    }

    /// Inserts or replaces a named saved library filter query.
    ///
    /// # Arguments
    ///
    /// * `name` - Logical name (filter, secret, …).
    /// * `query` - Saved filter query string.
    ///
    /// # Returns
    ///
    /// `Result<SavedFilterRecord>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn upsert_saved_filter(&self, name: &str, query: &str) -> Result<SavedFilterRecord> {
        let now = now_str();
        let existing = saved_filters::Entity::find()
            .filter(saved_filters::Column::Name.eq(name))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let model = if let Some(model) = existing {
            let mut am: saved_filters::ActiveModel = model.into();
            am.query = Set(query.to_string());
            am.updated_at = Set(now);
            am.update(&self.db).await.map_err(LibraryError::Orm)?
        } else {
            let am = saved_filters::ActiveModel {
                id: NotSet,
                name: Set(name.to_string()),
                query: Set(query.to_string()),
                created_at: Set(now.clone()),
                updated_at: Set(now),
            };
            am.insert(&self.db).await.map_err(LibraryError::Orm)?
        };
        Ok(map_saved_filter(model))
    }

    /// Loads a saved filter by name, if present.
    ///
    /// # Arguments
    ///
    /// * `name` - Logical name (filter, secret, …).
    ///
    /// # Returns
    ///
    /// `Result<Option<SavedFilterRecord>>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn get_saved_filter(&self, name: &str) -> Result<Option<SavedFilterRecord>> {
        Ok(saved_filters::Entity::find()
            .filter(saved_filters::Column::Name.eq(name))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_saved_filter))
    }

    /// Deletes a saved filter by name.
    ///
    /// # Arguments
    ///
    /// * `name` - Logical name (filter, secret, …).
    ///
    /// # Returns
    ///
    /// `Result<()>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn delete_saved_filter(&self, name: &str) -> Result<()> {
        let res = saved_filters::Entity::delete_many()
            .filter(saved_filters::Column::Name.eq(name))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if res.rows_affected == 0 {
            return Err(LibraryError::NotFound(name.into()));
        }
        Ok(())
    }

    /// Counts books whose acquire status equals `status`.
    ///
    /// A peer writer on the same SQLite file (TRUNCATE journal) can make this
    /// read return `SQLITE_BUSY`. The read is repeated until the lock clears
    /// or the attempt budget is spent. `status` is unchanged.
    ///
    /// # Arguments
    ///
    /// * `status` - Acquire or request status value.
    ///
    /// # Returns
    ///
    /// `Result<i64>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails for a reason other
    /// than file-lock contention, and [`LibraryError::Unavailable`] when the
    /// lock is still held after the retries.
    pub async fn count_by_status(&self, status: AcquireStatus) -> Result<i64> {
        lock_retry::retry_read_lock(|| async {
            let count = books::Entity::find()
                .filter(books::Column::AcquireStatus.eq(status.as_str()))
                .count(&self.db)
                .await
                .map_err(LibraryError::from_db_err)?;
            Ok(count as i64)
        })
        .await
    }

    /// Counts `books` rows per acquire status in one grouped read.
    ///
    /// # Returns
    ///
    /// The count for each [`AcquireStatus`] and the table total. A row with
    /// a status this build does not know is counted in the total only.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn count_books_by_status(&self) -> Result<AcquireStatusCounts> {
        /// One group of the read, decoded by column name: the host database
        /// proxy does not keep the select order, so a positional tuple
        /// read could swap the columns.
        #[derive(Debug, sea_orm::FromQueryResult)]
        struct StatusCount {
            /// The group's `acquire_status` wire string.
            acquire_status: String,
            /// Rows in the group.
            n: i64,
        }
        lock_retry::retry_read_lock(|| async {
            let rows = books::Entity::find()
                .select_only()
                .column(books::Column::AcquireStatus)
                .column_as(books::Column::Id.count(), "n")
                .group_by(books::Column::AcquireStatus)
                .into_model::<StatusCount>()
                .all(&self.db)
                .await
                .map_err(LibraryError::from_db_err)?;
            let mut counts = AcquireStatusCounts::default();
            for StatusCount {
                acquire_status: status,
                n,
            } in rows
            {
                counts.total += n;
                match AcquireStatus::parse(&status) {
                    Some(AcquireStatus::NotAcquired) => counts.not_acquired += n,
                    Some(AcquireStatus::Queued) => counts.queued += n,
                    Some(AcquireStatus::Downloading) => counts.downloading += n,
                    Some(AcquireStatus::Acquired) => counts.acquired += n,
                    Some(AcquireStatus::Error) => counts.error += n,
                    None => {}
                }
            }
            Ok(counts)
        })
        .await
    }

    /// Persist enrichment fields without touching scan / ownership columns.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn update_catalog_enrichment(
        &self,
        book_uuid: &str,
        fields: &CatalogEnrichmentFields,
    ) -> Result<()> {
        let model = books::Entity::find()
            .filter(books::Column::Uuid.eq(book_uuid))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound(book_uuid.into()))?;
        let mut am: books::ActiveModel = model.into();
        if let Some(v) = &fields.description {
            am.description = Set(Some(v.clone()));
        }
        if let Some(v) = &fields.language {
            am.language = Set(Some(v.clone()));
        }
        if let Some(v) = &fields.cover_url {
            am.cover_url = Set(Some(v.clone()));
        }
        if let Some(v) = &fields.subjects {
            am.subjects = Set(Some(v.clone()));
        }
        if let Some(v) = &fields.categories {
            am.categories = Set(Some(v.clone()));
        }
        if let Some(v) = &fields.enrich_source {
            am.enrich_source = Set(Some(v.clone()));
        }
        if let Some(v) = fields.enrich_confidence {
            am.enrich_confidence = Set(Some(v));
        }
        if let Some(v) = fields.enrich_updated_at {
            am.enrich_updated_at = Set(Some(v.to_rfc3339()));
        }
        am.updated_at = Set(now_str());
        let updated = self.update_book_bumped(am).await?;
        // The title's work takes the enrichment where it has none yet.
        if let Some(edition_id) = self.edition_id_for_book(&updated).await? {
            let fill = WorkEnrichment {
                description: updated.description,
                subjects: updated.subjects,
                categories: updated.categories,
                cover_url: updated.cover_url,
            };
            self.fill_work_from_book(&edition_id, &fill).await?;
        }
        Ok(())
    }

    /// Inserts or updates listening progress for a title.
    ///
    /// # Arguments
    ///
    /// * `row` - `row` argument for this call.
    ///
    /// # Returns
    ///
    /// `Result<ListeningProgressRecord>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn upsert_listening_progress(
        &self,
        row: &NewListeningProgress,
    ) -> Result<ListeningProgressRecord> {
        let now = now_str();
        let existing = listening_progress::Entity::find()
            .filter(listening_progress::Column::Provider.eq(row.provider.as_str()))
            .filter(listening_progress::Column::ExternalUserId.eq(row.external_user_id.as_str()))
            .filter(listening_progress::Column::ExternalItemId.eq(row.external_item_id.as_str()))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let model = if let Some(existing) = existing {
            let coalesce = |new: Option<String>, old: Option<String>| new.or(old);
            let mut am: listening_progress::ActiveModel = existing.clone().into();
            am.identity_id = Set(row.identity_id.or(existing.identity_id));
            am.work_id = Set(row.work_id.clone());
            am.edition_id = Set(coalesce(
                row.edition_id.clone(),
                existing.edition_id.clone(),
            ));
            am.progress = Set(row.progress);
            am.current_time_seconds = Set(row.current_time_seconds);
            am.duration_seconds = Set(row.duration_seconds);
            am.is_finished = Set(i64::from(row.is_finished));
            am.last_listened_at = Set(row
                .last_listened_at
                .map(|d| d.to_rfc3339())
                .or(existing.last_listened_at.clone()));
            am.updated_at = Set(now);
            am.update(&self.db).await.map_err(LibraryError::Orm)?
        } else {
            let am = listening_progress::ActiveModel {
                id: NotSet,
                identity_id: Set(row.identity_id),
                provider: Set(row.provider.clone()),
                external_user_id: Set(row.external_user_id.clone()),
                work_id: Set(row.work_id.clone()),
                edition_id: Set(row.edition_id.clone()),
                external_item_id: Set(row.external_item_id.clone()),
                progress: Set(row.progress),
                current_time_seconds: Set(row.current_time_seconds),
                duration_seconds: Set(row.duration_seconds),
                is_finished: Set(i64::from(row.is_finished)),
                last_listened_at: Set(row.last_listened_at.map(|d| d.to_rfc3339())),
                updated_at: Set(now),
            };
            am.insert(&self.db).await.map_err(LibraryError::Orm)?
        };
        Ok(map_listening(model))
    }

    /// Loads listening progress for one identity and book.
    ///
    /// # Arguments
    ///
    /// * `provider` - External provider id.
    /// * `external_user_id` - User id at the external provider.
    /// * `external_item_id` - `external_item_id` argument for this call.
    ///
    /// # Returns
    ///
    /// `Result<Option<ListeningProgressRecord>>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn get_listening_progress(
        &self,
        provider: &str,
        external_user_id: &str,
        external_item_id: &str,
    ) -> Result<Option<ListeningProgressRecord>> {
        Ok(listening_progress::Entity::find()
            .filter(listening_progress::Column::Provider.eq(provider))
            .filter(listening_progress::Column::ExternalUserId.eq(external_user_id))
            .filter(listening_progress::Column::ExternalItemId.eq(external_item_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_listening))
    }

    /// Lists listening progress rows for an identity.
    ///
    /// # Arguments
    ///
    /// * `external_user_id` - User id at the external provider.
    ///
    /// # Returns
    ///
    /// `Result<Vec<ListeningProgressRecord>>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn list_listening_progress(
        &self,
        external_user_id: Option<&str>,
    ) -> Result<Vec<ListeningProgressRecord>> {
        let mut query = listening_progress::Entity::find();
        if let Some(uid) = external_user_id {
            query = query.filter(listening_progress::Column::ExternalUserId.eq(uid));
        }
        let mut rows: Vec<ListeningProgressRecord> = query
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_listening)
            .collect();
        // ORDER BY COALESCE(last_listened_at, updated_at) DESC.
        rows.sort_by(|a, b| {
            let ka = a.last_listened_at.unwrap_or(a.updated_at);
            let kb = b.last_listened_at.unwrap_or(b.updated_at);
            kb.cmp(&ka)
        });
        Ok(rows)
    }

    /// Creates a wishlist / title-request row.
    ///
    /// # Arguments
    ///
    /// * `req` - Insert / update request payload.
    ///
    /// # Returns
    ///
    /// `Result<TitleRequestRecord>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn create_title_request(&self, req: &NewTitleRequest) -> Result<TitleRequestRecord> {
        self.create_title_request_for(req, None).await
    }

    /// Creates a wishlist / title-request row for a known work.
    ///
    /// A wish that names a store edition the server looked up is for that
    /// listing's work: its resolution in the shared pool, with no holding.
    /// Without one, the wish's text and identifiers find or make the work
    /// ([`Self::work_for_wish`]).
    ///
    /// # Arguments
    ///
    /// * `req` - Insert / update request payload.
    /// * `work` - The work (and edition) of a pooled listing the wish
    ///   names, or `None` to resolve it from `req`.
    ///
    /// # Returns
    ///
    /// The new wish, or the caller's open wish for the same work.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn create_title_request_for(
        &self,
        req: &NewTitleRequest,
        work: Option<&WishWork>,
    ) -> Result<TitleRequestRecord> {
        let decoded;
        let req = if req.needs_html_entity_decode() {
            decoded = {
                let mut r = req.clone();
                r.decode_html_entities();
                r
            };
            &decoded
        } else {
            req
        };
        let now = now_str();
        let uuid = req
            .uuid
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        // The wish names its work; wishing twice for one work is one wish.
        let (work_id, edition_id) = match work {
            Some(work) => (work.work_id.clone(), work.edition_id.clone()),
            None => {
                self.work_for_wish(&crate::TitleQuery {
                    title: &req.title,
                    authors: req.authors.as_deref(),
                    asin: req.asin.as_deref(),
                    isbn: req.isbn.as_deref(),
                    series: None,
                    series_index: None,
                })
                .await?
            }
        };
        if let Some(existing) = self.find_open_wishlist(req.identity_id, &work_id).await? {
            return self
                .backfill_title_request_cover(&existing, req.cover_url.as_deref())
                .await;
        }

        let am = title_requests::ActiveModel {
            id: NotSet,
            uuid: Set(uuid),
            identity_id: Set(req.identity_id),
            work_id: Set(work_id),
            edition_id: Set(edition_id),
            notes: Set(req.notes.clone()),
            status: Set(req.status.as_str().to_string()),
            preferred_source: NotSet,
            resolved_book_uuid: Set(req.resolved_book_uuid.clone()),
            cover_url: Set(req.cover_url.clone()),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        };
        let model = am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        self.publish_wishlist_changed(&model.uuid, model.identity_id, &model.status)
            .await;
        self.get_title_request_by_id(model.id)
            .await?
            .ok_or_else(|| LibraryError::NotFound(model.uuid))
    }

    /// Publishes `wishlist_changed` for one wishlist row, after its write.
    ///
    /// Best effort: the row is already committed, so a failed publish is
    /// logged rather than undoing it. Discover's daily refresh still picks
    /// the change up.
    ///
    /// # Arguments
    ///
    /// * `uuid` - The wishlist row's uuid.
    /// * `identity_id` - The portal identity that wished for it, when any.
    /// * `status` - The row's status after the write (`open` or `cancelled`),
    ///   or `deleted` when the row was removed with its user.
    async fn publish_wishlist_changed(&self, uuid: &str, identity_id: Option<i64>, status: &str) {
        let payload = serde_json::json!({
            "requestId": uuid,
            "identityId": identity_id,
            "status": status,
        });
        let spec = crate::PublishDomainEventSpec {
            event_type: crate::WISHLIST_CHANGED.into(),
            schema_version: 1,
            correlation_id: uuid.to_string(),
            // Each write is its own change; a reopened wish is a new event.
            dedup_key: format!("{}:{uuid}:{}", crate::WISHLIST_CHANGED, Uuid::new_v4()),
            payload: payload.to_string(),
            ordering_key: uuid.to_string(),
            ..crate::PublishDomainEventSpec::default()
        };
        if let Err(err) = self.publish_domain_event(spec).await {
            tracing::warn!(request = %uuid, error = %err, "could not publish wishlist_changed");
        }
    }

    /// Upsert per-storefront snapshots for a wishlist row, then return the merged record.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn upsert_title_request_sources(
        &self,
        title_request_id: i64,
        sources: &[NewTitleRequestSource],
    ) -> Result<TitleRequestRecord> {
        let now = now_str();
        for src in sources {
            let decoded;
            let src = if src.needs_html_entity_decode() {
                decoded = {
                    let mut s = src.clone();
                    s.decode_html_entities();
                    s
                };
                &decoded
            } else {
                src
            };
            let source = src.source.trim().to_ascii_lowercase();
            let product_id = src.product_id.trim();
            if source.is_empty() || product_id.is_empty() {
                continue;
            }
            let existing = title_request_sources::Entity::find()
                .filter(title_request_sources::Column::TitleRequestId.eq(title_request_id))
                .filter(title_request_sources::Column::Source.eq(&source))
                .filter(title_request_sources::Column::ProductId.eq(product_id))
                .one(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            if let Some(model) = existing {
                let am = merged_source_active(model, src, &now);
                am.update(&self.db).await.map_err(LibraryError::Orm)?;
            } else {
                let am = title_request_sources::ActiveModel {
                    id: NotSet,
                    title_request_id: Set(title_request_id),
                    source: Set(source),
                    product_id: Set(product_id.to_string()),
                    title: Set(trim_opt(src.title.as_deref())),
                    subtitle: Set(trim_opt(src.subtitle.as_deref())),
                    authors: Set(trim_opt(src.authors.as_deref())),
                    narrators: Set(trim_opt(src.narrators.as_deref())),
                    series: Set(trim_opt(src.series.as_deref())),
                    series_index: Set(trim_opt(src.series_index.as_deref())),
                    asin: Set(trim_opt(src.asin.as_deref())),
                    isbn: Set(trim_opt(src.isbn.as_deref())),
                    description: Set(trim_opt(src.description.as_deref())),
                    publisher: Set(trim_opt(src.publisher.as_deref())),
                    length_minutes: Set(src.length_minutes),
                    published_at: Set(trim_opt(src.published_at.as_deref())),
                    categories: Set(trim_opt(src.categories.as_deref())),
                    language: Set(trim_opt(src.language.as_deref())),
                    cover_url: Set(trim_opt(src.cover_url.as_deref())),
                    url: Set(trim_opt(src.url.as_deref())),
                    price_cents: Set(src.price_cents),
                    currency: Set(trim_opt(src.currency.as_deref())),
                    price_label: Set(trim_opt(src.price_label.as_deref())),
                    list_price_cents: Set(src.list_price_cents),
                    list_price_label: Set(trim_opt(src.list_price_label.as_deref())),
                    member_price_cents: Set(src.member_price_cents),
                    member_price_label: Set(trim_opt(src.member_price_label.as_deref())),
                    observed_at: Set(Some(now.clone())),
                    listing_id: Set(src.listing_id.clone()),
                    created_at: Set(now.clone()),
                    updated_at: Set(now.clone()),
                };
                am.insert(&self.db).await.map_err(LibraryError::Orm)?;
            }
        }
        let mut row = self
            .get_title_request_by_id(title_request_id)
            .await?
            .ok_or_else(|| LibraryError::NotFound(title_request_id.to_string()))?;
        // Persist merged identity fields onto the parent when still blank.
        if let Some(model) = title_requests::Entity::find_by_id(title_request_id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        {
            let mut am: title_requests::ActiveModel = model.clone().into();
            let mut dirty = false;
            if model
                .cover_url
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_none()
            {
                if let Some(c) = row
                    .cover_url
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    am.cover_url = Set(Some(c.to_string()));
                    dirty = true;
                }
            }
            if dirty {
                am.updated_at = Set(now_str());
                am.update(&self.db).await.map_err(LibraryError::Orm)?;
                row = self
                    .get_title_request_by_id(title_request_id)
                    .await?
                    .unwrap_or(row);
            }
        }
        Ok(row)
    }

    /// Lists per-storefront snapshots for a title request.
    ///
    /// # Arguments
    ///
    /// * `title_request_id` - `title_request_id` argument for this call.
    ///
    /// # Returns
    ///
    /// `Result<Vec<TitleRequestSourceRecord>>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn list_title_request_sources(
        &self,
        title_request_id: i64,
    ) -> Result<Vec<TitleRequestSourceRecord>> {
        Ok(title_request_sources::Entity::find()
            .filter(title_request_sources::Column::TitleRequestId.eq(title_request_id))
            .order_by_asc(title_request_sources::Column::Source)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_source)
            .collect())
    }

    /// Loads one wishlist title request and attaches merged storefront sources.
    async fn get_title_request_by_id(&self, id: i64) -> Result<Option<TitleRequestRecord>> {
        let Some(model) = title_requests::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        Ok(self.hydrate_requests(vec![model]).await?.pop())
    }

    /// Turns stored wish rows into records: each wish's title and authors
    /// from its work, its storefront sources merged in, and the card key
    /// derived from both. Works, credits and sources are read in chunked
    /// bulk lookups, not per wish.
    ///
    /// # Arguments
    ///
    /// * `models` - Stored wish rows, in the order to return them.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    async fn hydrate_requests(
        &self,
        models: Vec<title_requests::Model>,
    ) -> Result<Vec<TitleRequestRecord>> {
        if models.is_empty() {
            return Ok(Vec::new());
        }
        let work_ids: Vec<String> = models
            .iter()
            .map(|m| m.work_id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let works = self.works_by_ids(&work_ids).await?;
        let edition_ids: Vec<String> = models
            .iter()
            .filter_map(|m| m.edition_id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let edition_codes = self.edition_asin_isbn(&edition_ids).await?;
        let ids: Vec<i64> = models.iter().map(|m| m.id).collect();
        let mut by_id: std::collections::HashMap<i64, Vec<TitleRequestSourceRecord>> =
            std::collections::HashMap::new();
        for part in ids.chunks(self.lookup_chunk()) {
            let found = title_request_sources::Entity::find()
                .filter(title_request_sources::Column::TitleRequestId.is_in(part.iter().copied()))
                .order_by_asc(title_request_sources::Column::Source)
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            for m in found {
                by_id
                    .entry(m.title_request_id)
                    .or_default()
                    .push(map_source(m));
            }
        }
        Ok(models
            .into_iter()
            .map(|model| {
                let id = model.id;
                let mut row = map_request(model);
                if let Some(work) = works.get(&row.work_id) {
                    row.title.clone_from(&work.title);
                    row.authors = (!work.credits.is_empty()).then(|| work.credits.join(", "));
                }
                row.sources = by_id.remove(&id).unwrap_or_default();
                apply_merged_sources(&mut row);
                // The edition the wish named carries the identifiers it was
                // resolved from, when no store source names them.
                if let Some((asin, isbn)) = row
                    .edition_id
                    .as_ref()
                    .and_then(|edition| edition_codes.get(edition))
                {
                    if row.asin.is_none() {
                        row.asin.clone_from(asin);
                    }
                    if row.isbn.is_none() {
                        row.isbn.clone_from(isbn);
                    }
                }
                row.work_key = fallback_work_key(
                    &row.title,
                    row.authors.as_deref(),
                    row.asin.as_deref(),
                    row.isbn.as_deref(),
                );
                row
            })
            .collect())
    }

    /// The first ASIN and ISBN each edition carries, read in chunked bulk
    /// queries.
    ///
    /// # Arguments
    ///
    /// * `edition_ids` - Edition ids to read.
    ///
    /// # Returns
    ///
    /// `(asin, isbn)` per edition that carries either; values are taken in
    /// value order so the answer is stable.
    ///
    /// # Errors
    ///
    /// Returns the database read error.
    async fn edition_asin_isbn(
        &self,
        edition_ids: &[String],
    ) -> Result<std::collections::HashMap<String, (Option<String>, Option<String>)>> {
        let mut out: std::collections::HashMap<String, (Option<String>, Option<String>)> =
            std::collections::HashMap::new();
        for part in edition_ids.chunks(self.lookup_chunk()) {
            let rows = crate::entities::edition_identifiers::Entity::find()
                .filter(
                    crate::entities::edition_identifiers::Column::EditionId
                        .is_in(part.iter().cloned()),
                )
                .filter(crate::entities::edition_identifiers::Column::Scheme.is_in([
                    bookclerk_resolve::IdentifierScheme::Asin.as_str(),
                    bookclerk_resolve::IdentifierScheme::Isbn.as_str(),
                ]))
                .order_by_asc(crate::entities::edition_identifiers::Column::Value)
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            for m in rows {
                let entry = out.entry(m.edition_id).or_default();
                let slot = if m.scheme == bookclerk_resolve::IdentifierScheme::Asin.as_str() {
                    &mut entry.0
                } else {
                    &mut entry.1
                };
                if slot.is_none() {
                    *slot = Some(m.value);
                }
            }
        }
        Ok(out)
    }

    /// Fill `cover_url` on an open wishlist row when the client later supplies one.
    async fn backfill_title_request_cover(
        &self,
        existing: &TitleRequestRecord,
        cover_url: Option<&str>,
    ) -> Result<TitleRequestRecord> {
        let cover = cover_url.map(str::trim).filter(|s| !s.is_empty());
        let Some(cover) = cover else {
            return Ok(existing.clone());
        };
        if existing
            .cover_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some()
        {
            return Ok(existing.clone());
        }
        let model = title_requests::Entity::find_by_id(existing.id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound(existing.uuid.clone()))?;
        let mut am: title_requests::ActiveModel = model.into();
        am.cover_url = Set(Some(cover.to_string()));
        am.updated_at = Set(now_str());
        let model = am.update(&self.db).await.map_err(LibraryError::Orm)?;
        self.hydrate_requests(vec![model])
            .await?
            .pop()
            .ok_or_else(|| LibraryError::NotFound(existing.uuid.clone()))
    }

    /// Open wish of this identity for `work_id`, if any.
    ///
    /// # Arguments
    ///
    /// * `identity_id` - The wisher's portal identity; `None` for the operator.
    /// * `work_id` - The work wished for.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn find_open_wishlist(
        &self,
        identity_id: Option<i64>,
        work_id: &str,
    ) -> Result<Option<TitleRequestRecord>> {
        let mut query = title_requests::Entity::find()
            .filter(title_requests::Column::WorkId.eq(work_id))
            .filter(title_requests::Column::Status.eq(RequestStatus::Open.as_str()))
            .order_by_desc(title_requests::Column::CreatedAt);
        query = match identity_id {
            Some(id) => query.filter(title_requests::Column::IdentityId.eq(id)),
            None => query.filter(title_requests::Column::IdentityId.is_null()),
        };
        let Some(model) = query.one(&self.db).await.map_err(LibraryError::Orm)? else {
            return Ok(None);
        };
        Ok(self.hydrate_requests(vec![model]).await?.pop())
    }

    /// Personal open wishlist for a portal identity, or operator (`identity_id` null).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn list_wishlist(&self, identity_id: Option<i64>) -> Result<Vec<TitleRequestRecord>> {
        let mut query = title_requests::Entity::find()
            .filter(title_requests::Column::Status.eq(RequestStatus::Open.as_str()))
            .order_by_desc(title_requests::Column::CreatedAt);
        query = match identity_id {
            Some(id) => query.filter(title_requests::Column::IdentityId.eq(id)),
            None => query.filter(title_requests::Column::IdentityId.is_null()),
        };
        let rows = query.all(&self.db).await.map_err(LibraryError::Orm)?;
        self.hydrate_requests(rows).await
    }

    /// Global request queue: open wishes grouped by their work.
    ///
    /// Sorted by wish count as a simple default; Discover re-ranks with local
    /// taste plus a heavy per-wisher boost for the Wishlist sidebar.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn list_global_request_queue(&self) -> Result<Vec<GlobalQueueEntry>> {
        let open = self.list_title_requests(Some(RequestStatus::Open)).await?;
        let identity_ids: Vec<i64> = open.iter().filter_map(|r| r.identity_id).collect();
        let identities = self.portal_identities_by_ids(&identity_ids).await?;
        let user_ids: Vec<i64> = identities.values().filter_map(|i| i.user_id).collect();
        let users = self.users_by_ids(&user_ids).await?;
        let mut by_key: std::collections::HashMap<String, GlobalQueueEntry> =
            std::collections::HashMap::new();
        for row in open {
            let wisher = queue_wisher_for(row.identity_id, &identities, &users);
            match by_key.entry(row.work_id.clone()) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    let mut entry = GlobalQueueEntry {
                        work_key: row.work_key,
                        work_id: row.work_id,
                        title: row.title,
                        authors: row.authors,
                        asin: row.asin,
                        isbn: row.isbn,
                        cover_url: row.cover_url,
                        description: row.description,
                        subtitle: row.subtitle,
                        narrators: row.narrators,
                        series: row.series,
                        series_index: row.series_index,
                        publisher: row.publisher,
                        length_minutes: row.length_minutes,
                        published_at: row.published_at,
                        genres: row.genres,
                        language: row.language,
                        store_editions: row.store_editions,
                        purchase_hints: row.purchase_hints,
                        wish_count: 1,
                        wishers: Vec::new(),
                        sample_uuids: vec![row.uuid],
                        first_requested_at: row.created_at,
                        last_requested_at: row.created_at,
                    };
                    entry.push_wisher(wisher);
                    e.insert(entry);
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    let entry = e.get_mut();
                    entry.wish_count += 1;
                    entry.push_wisher(wisher);
                    if entry.sample_uuids.len() < 8 {
                        entry.sample_uuids.push(row.uuid);
                    }
                    if entry.cover_url.as_deref().unwrap_or("").trim().is_empty()
                        && !row.cover_url.as_deref().unwrap_or("").trim().is_empty()
                    {
                        entry.cover_url = row.cover_url.clone();
                    }
                    entry.description = crate::wishlist_merge::pick_better_description(
                        entry.description.as_deref(),
                        row.description.as_deref(),
                    );
                    for ed in row.store_editions {
                        if !entry
                            .store_editions
                            .iter()
                            .any(|e| e.source == ed.source && e.product_id == ed.product_id)
                        {
                            entry.store_editions.push(ed);
                        }
                    }
                    for hint in row.purchase_hints {
                        if let Some(existing) = entry
                            .purchase_hints
                            .iter_mut()
                            .find(|h| h.source == hint.source && h.product_id == hint.product_id)
                        {
                            if existing.price_cents.is_none() {
                                existing.price_cents = hint.price_cents;
                            }
                            if existing
                                .url
                                .as_deref()
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .is_none()
                            {
                                existing.url = hint.url;
                            }
                            if existing.price_label.is_none() {
                                existing.price_label = hint.price_label;
                            }
                            if existing.list_price_cents.is_none() {
                                existing.list_price_cents = hint.list_price_cents;
                            }
                            if existing.member_price_cents.is_none() {
                                existing.member_price_cents = hint.member_price_cents;
                            }
                        } else {
                            entry.purchase_hints.push(hint);
                        }
                    }
                    if row.created_at < entry.first_requested_at {
                        entry.first_requested_at = row.created_at;
                    }
                    if row.created_at > entry.last_requested_at {
                        entry.last_requested_at = row.created_at;
                        // Prefer the newest metadata.
                        entry.work_key = row.work_key;
                        entry.title = row.title;
                        if row.authors.is_some() {
                            entry.authors = row.authors;
                        }
                        if row.asin.is_some() {
                            entry.asin = row.asin;
                        }
                        if row.isbn.is_some() {
                            entry.isbn = row.isbn;
                        }
                        if !row.cover_url.as_deref().unwrap_or("").trim().is_empty() {
                            entry.cover_url = row.cover_url;
                        }
                        if row.subtitle.is_some() {
                            entry.subtitle = row.subtitle;
                        }
                        if row.narrators.is_some() {
                            entry.narrators = row.narrators;
                        }
                        if row.series.is_some() {
                            entry.series = row.series;
                        }
                        if row.series_index.is_some() {
                            entry.series_index = row.series_index;
                        }
                        if row.publisher.is_some() {
                            entry.publisher = row.publisher;
                        }
                        if row.length_minutes.is_some() {
                            entry.length_minutes = row.length_minutes;
                        }
                        if row.published_at.is_some() {
                            entry.published_at = row.published_at;
                        }
                        if row.genres.is_some() {
                            entry.genres = row.genres;
                        }
                        if row.language.is_some() {
                            entry.language = row.language;
                        }
                    }
                }
            }
        }
        let mut out: Vec<_> = by_key.into_values().collect();
        out.sort_by(|a, b| {
            b.wish_count
                .cmp(&a.wish_count)
                .then_with(|| b.last_requested_at.cmp(&a.last_requested_at))
        });
        Ok(out)
    }

    /// Loads a wishlist title-request row by UUID, if present.
    ///
    /// # Arguments
    ///
    /// * `uuid` - Book UUID.
    ///
    /// # Returns
    ///
    /// `Result<Option<TitleRequestRecord>>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn get_title_request_by_uuid(
        &self,
        uuid: &str,
    ) -> Result<Option<TitleRequestRecord>> {
        let Some(model) = title_requests::Entity::find()
            .filter(title_requests::Column::Uuid.eq(uuid))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        Ok(self.hydrate_requests(vec![model]).await?.pop())
    }

    /// Lists wishlist / title-request rows.
    ///
    /// # Arguments
    ///
    /// * `status` - Acquire or request status value.
    ///
    /// # Returns
    ///
    /// `Result<Vec<TitleRequestRecord>>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn list_title_requests(
        &self,
        status: Option<RequestStatus>,
    ) -> Result<Vec<TitleRequestRecord>> {
        let mut query =
            title_requests::Entity::find().order_by_desc(title_requests::Column::CreatedAt);
        if let Some(status) = status {
            query = query.filter(title_requests::Column::Status.eq(status.as_str()));
        }
        let rows = query.all(&self.db).await.map_err(LibraryError::Orm)?;
        self.hydrate_requests(rows).await
    }

    /// Updates the lifecycle status of a title request.
    ///
    /// # Arguments
    ///
    /// * `uuid` - Book UUID.
    /// * `status` - Acquire or request status value.
    /// * `resolved_book_uuid` - `resolved_book_uuid` argument for this call.
    ///
    /// # Returns
    ///
    /// `Result<()>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn update_title_request_status(
        &self,
        uuid: &str,
        status: RequestStatus,
        resolved_book_uuid: Option<&str>,
    ) -> Result<()> {
        let model = title_requests::Entity::find()
            .filter(title_requests::Column::Uuid.eq(uuid))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound(uuid.into()))?;
        let mut am: title_requests::ActiveModel = model.into();
        am.status = Set(status.as_str().to_string());
        if let Some(v) = resolved_book_uuid {
            am.resolved_book_uuid = Set(Some(v.to_string()));
        }
        am.updated_at = Set(now_str());
        let model = am.update(&self.db).await.map_err(LibraryError::Orm)?;
        self.publish_wishlist_changed(&model.uuid, model.identity_id, &model.status)
            .await;
        Ok(())
    }

    /// Inserts or replaces an embedding vector for a target.
    ///
    /// # Arguments
    ///
    /// * `target_kind` - Embedding target kind.
    /// * `target_id` - Embedding target id.
    /// * `model` - Embedding model id.
    /// * `dims` - Vector dimensionality.
    /// * `vector` - Raw embedding bytes.
    /// * `text_hash` - Hash of the embedded text.
    ///
    /// # Returns
    ///
    /// `Result<()>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn upsert_embedding(
        &self,
        target_kind: &str,
        target_id: &str,
        model: &str,
        dims: i64,
        vector: &[u8],
        text_hash: &str,
    ) -> Result<()> {
        let now = now_str();
        let existing = embeddings::Entity::find()
            .filter(embeddings::Column::TargetKind.eq(target_kind))
            .filter(embeddings::Column::TargetId.eq(target_id))
            .filter(embeddings::Column::Model.eq(model))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if let Some(existing) = existing {
            let mut am: embeddings::ActiveModel = existing.into();
            am.dims = Set(dims);
            am.vector = Set(vector.to_vec());
            am.text_hash = Set(text_hash.to_string());
            am.updated_at = Set(now);
            am.update(&self.db).await.map_err(LibraryError::Orm)?;
        } else {
            let am = embeddings::ActiveModel {
                id: NotSet,
                target_kind: Set(target_kind.to_string()),
                target_id: Set(target_id.to_string()),
                model: Set(model.to_string()),
                dims: Set(dims),
                vector: Set(vector.to_vec()),
                text_hash: Set(text_hash.to_string()),
                created_at: Set(now.clone()),
                updated_at: Set(now),
            };
            am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        }
        Ok(())
    }

    /// Loads the raw embedding bytes for a target, if present.
    ///
    /// # Arguments
    ///
    /// * `target_kind` - Embedding target kind.
    /// * `target_id` - Embedding target id.
    /// * `model` - Embedding model id.
    ///
    /// # Returns
    ///
    /// `Result<Option<(String, Vec<u8>)>>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn get_embedding_vector(
        &self,
        target_kind: &str,
        target_id: &str,
        model: &str,
    ) -> Result<Option<(String, Vec<u8>)>> {
        Ok(embeddings::Entity::find()
            .filter(embeddings::Column::TargetKind.eq(target_kind))
            .filter(embeddings::Column::TargetId.eq(target_id))
            .filter(embeddings::Column::Model.eq(model))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(|m| (m.text_hash, m.vector)))
    }

    /// Lists embedding metadata rows (optionally filtered).
    ///
    /// # Arguments
    ///
    /// * `target_kind` - Embedding target kind.
    /// * `model` - Embedding model id.
    ///
    /// # Returns
    ///
    /// `Result<Vec<(String, Vec<u8>)>>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn list_embeddings(
        &self,
        target_kind: &str,
        model: &str,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        Ok(embeddings::Entity::find()
            .filter(embeddings::Column::TargetKind.eq(target_kind))
            .filter(embeddings::Column::Model.eq(model))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(|m| (m.target_id, m.vector))
            .collect())
    }

    /// Returns the stored text hash for an embedding target, if any.
    ///
    /// # Arguments
    ///
    /// * `target_kind` - Embedding target kind.
    /// * `target_id` - Embedding target id.
    /// * `model` - Embedding model id.
    ///
    /// # Returns
    ///
    /// `Result<Option<String>>` — `Ok` on success.
    ///
    /// # Errors
    ///
    /// Returns a crate error when the database operation fails or inputs are invalid.
    pub async fn embedding_text_hash(
        &self,
        target_kind: &str,
        target_id: &str,
        model: &str,
    ) -> Result<Option<String>> {
        Ok(embeddings::Entity::find()
            .filter(embeddings::Column::TargetKind.eq(target_kind))
            .filter(embeddings::Column::TargetId.eq(target_id))
            .filter(embeddings::Column::Model.eq(model))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(|m| m.text_hash))
    }

    /// Load per-user GUI / Discover preferences by subject key.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn get_user_preferences(&self, subject_key: &str) -> Result<Option<UserPreferences>> {
        Ok(user_preferences::Entity::find()
            .filter(user_preferences::Column::SubjectKey.eq(subject_key))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_user_preferences))
    }

    /// Preferences for `subject_key`, or in-memory defaults when no row exists.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn get_user_preferences_or_default(
        &self,
        subject_key: &str,
        identity_id: Option<i64>,
    ) -> Result<UserPreferences> {
        Ok(self
            .get_user_preferences(subject_key)
            .await?
            .unwrap_or_else(|| UserPreferences::defaults_for(subject_key, identity_id)))
    }

    /// Insert or replace preferences for a subject (operator or portal identity).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_user_preferences(
        &self,
        subject_key: &str,
        identity_id: Option<i64>,
        default_view: &str,
        disabled_shelves: &[String],
        discover_sort: &str,
        discover_sort_dir: &str,
        discover_language: Option<&str>,
        discover_excluded_sources: &[String],
        theme: &str,
    ) -> Result<UserPreferences> {
        let now = now_str();
        let shelves_json =
            serde_json::to_string(disabled_shelves).unwrap_or_else(|_| String::from("[]"));
        let excluded_json =
            serde_json::to_string(discover_excluded_sources).unwrap_or_else(|_| String::from("[]"));
        let sort = normalize_discover_sort(discover_sort);
        let sort_dir = normalize_discover_sort_dir(discover_sort_dir);
        let appearance = crate::normalize_theme(theme);
        let language = discover_language
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let existing = user_preferences::Entity::find()
            .filter(user_preferences::Column::SubjectKey.eq(subject_key))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let model = if let Some(existing) = existing {
            let mut am: user_preferences::ActiveModel = existing.clone().into();
            if identity_id.is_some() {
                am.identity_id = Set(identity_id.or(existing.identity_id));
            }
            am.default_view = Set(default_view.to_string());
            am.disabled_shelves_json = Set(shelves_json);
            am.discover_sort = Set(sort);
            am.discover_sort_dir = Set(sort_dir);
            am.discover_language = Set(language);
            am.discover_excluded_sources_json = Set(excluded_json);
            am.theme = Set(appearance);
            am.updated_at = Set(now);
            am.update(&self.db).await.map_err(LibraryError::Orm)?
        } else {
            let am = user_preferences::ActiveModel {
                id: NotSet,
                subject_key: Set(subject_key.to_string()),
                identity_id: Set(identity_id),
                default_view: Set(default_view.to_string()),
                disabled_shelves_json: Set(shelves_json),
                discover_sort: Set(sort),
                discover_sort_dir: Set(sort_dir),
                discover_language: Set(language),
                discover_excluded_sources_json: Set(excluded_json),
                theme: Set(appearance),
                updated_at: Set(now),
            };
            am.insert(&self.db).await.map_err(LibraryError::Orm)?
        };
        Ok(map_user_preferences(model))
    }
}

/// Match precedence for [`LibraryStore::get_book`]: uuid, product_id, asin, else.
fn match_priority(book: &books::Model, needle: &str) -> u8 {
    if book.uuid == needle {
        0
    } else if book.product_id == needle {
        1
    } else if book.asin.as_deref() == Some(needle) {
        2
    } else {
        3
    }
}

/// Prefer an Audible ownership row with the richest metadata for enrichment.
#[must_use]
pub fn prefer_enrichment_source(books: &[BookRecord]) -> Option<&BookRecord> {
    books.iter().max_by_key(|b| {
        let mut score = 0u32;
        // Prefer rows that already carry an Audible ASIN (ownership or enrichment).
        if b.asin.is_some() {
            score += 100;
        }
        if b.isbn.is_some() {
            score += 10;
        }
        if b.authors.is_some() {
            score += 2;
        }
        if b.narrators.is_some() {
            score += 2;
        }
        if b.series.is_some() {
            score += 2;
        }
        if b.publisher.is_some() {
            score += 1;
        }
        if b.subtitle.is_some() {
            score += 1;
        }
        if b.length_minutes.is_some() {
            score += 1;
        }
        if b.categories.is_some() {
            score += 1;
        }
        if !b.title.is_empty() {
            score += 1;
        }
        score
    })
}

/// Input for inserting / updating a book from sync.
#[derive(Debug, Clone)]
pub struct NewBook {
    /// Stable UUID for this row (API / foreign-key identity).
    pub uuid: Option<String>,
    /// Storefront product id (ASIN, ISBN, UUID, …).
    pub product_id: String,
    /// Provenance-qualified PluginKey that owns the account. Forced by the
    /// scope on write.
    pub plugin_key: String,
    /// Display alias of the owning plugin (`audible`, `libro`, …).
    pub source: String,
    /// Store or operator account id this row belongs to.
    pub account_id: String,
    /// Amazon ASIN when known; otherwise null.
    pub asin: Option<String>,
    /// ISBN-10/13 when known; otherwise null.
    pub isbn: Option<String>,
    /// Store marketplace / locale code (for example `us`, `uk`).
    pub marketplace: String,
    /// Display title of the work or edition.
    pub title: String,
    /// Comma-separated or JSON author list from the storefront.
    pub authors: Option<String>,
    /// Comma-separated or JSON narrator list when present.
    pub narrators: Option<String>,
    /// Series name when the title belongs to a series.
    pub series: Option<String>,
    /// Position within the series (storefront string form).
    pub series_index: Option<String>,
    /// Amazon series ASIN when the storefront exposes one.
    pub series_asin: Option<String>,
    /// RFC 3339 purchase time from the storefront, when known.
    pub purchased_at: Option<chrono::DateTime<Utc>>,
    /// Publisher name from metadata enrichment or the storefront.
    pub publisher: Option<String>,
    /// Runtime in whole minutes when the storefront reports it.
    pub length_minutes: Option<i64>,
    /// Whether the edition is abridged (0/1 or bool).
    pub is_abridged: bool,
    /// Title kind: `book`, `episode`, `podcast`, ….
    pub content_kind: String,
    /// Storefront category / genre path list (serialized).
    pub categories: Option<String>,
    /// Optional subtitle from bibliographic metadata.
    pub subtitle: Option<String>,
    /// Publication date string from the storefront or enrichment.
    pub published_at: Option<chrono::DateTime<Utc>>,
}

impl NewBook {
    /// True when any human-readable field may contain an HTML entity.
    #[must_use]
    pub fn needs_html_entity_decode(&self) -> bool {
        crate::str_maybe_html_entity(&self.title)
            || self
                .authors
                .as_deref()
                .is_some_and(crate::str_maybe_html_entity)
            || self
                .narrators
                .as_deref()
                .is_some_and(crate::str_maybe_html_entity)
            || self
                .series
                .as_deref()
                .is_some_and(crate::str_maybe_html_entity)
            || self
                .publisher
                .as_deref()
                .is_some_and(crate::str_maybe_html_entity)
            || self
                .categories
                .as_deref()
                .is_some_and(crate::str_maybe_html_entity)
            || self
                .subtitle
                .as_deref()
                .is_some_and(crate::str_maybe_html_entity)
    }

    /// Decode HTML entities in human-readable metadata fields (titles, etc.).
    pub fn decode_html_entities(&mut self) {
        crate::decode_html_entities_in_place(&mut self.title);
        crate::decode_html_entities_opt_in_place(&mut self.authors);
        crate::decode_html_entities_opt_in_place(&mut self.narrators);
        crate::decode_html_entities_opt_in_place(&mut self.series);
        crate::decode_html_entities_opt_in_place(&mut self.publisher);
        crate::decode_html_entities_opt_in_place(&mut self.categories);
        crate::decode_html_entities_opt_in_place(&mut self.subtitle);
    }

    /// Minimal Audible book row with scan metadata defaults.
    ///
    /// Sets `product_id` from the first argument, `asin = Some(product_id)`,
    /// `isbn = None`, `uuid = None` (auto), and `plugin_key = source =
    /// "audible"` (a [`crate::SourceScope`] replaces both on write).
    #[must_use]
    pub fn minimal(
        product_id: impl Into<String>,
        account_id: impl Into<String>,
        marketplace: impl Into<String>,
        title: impl Into<String>,
    ) -> Self {
        let product_id = product_id.into();
        Self {
            uuid: None,
            asin: Some(product_id.clone()),
            isbn: None,
            product_id,
            plugin_key: String::from("audible"),
            source: String::from("audible"),
            account_id: account_id.into(),
            marketplace: marketplace.into(),
            title: title.into(),
            authors: None,
            narrators: None,
            series: None,
            series_index: None,
            series_asin: None,
            purchased_at: None,
            publisher: None,
            length_minutes: None,
            is_abridged: false,
            content_kind: String::from("book"),
            categories: None,
            subtitle: None,
            published_at: None,
        }
    }
}

/// Saved Lucene-style quick filter.
#[derive(Debug, Clone)]
pub struct SavedFilterRecord {
    /// Surrogate primary key assigned by the database.
    pub id: i64,
    /// Human-readable or logical name for this row.
    pub name: String,
    /// Saved library filter expression (UI search DSL).
    pub query: String,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: chrono::DateTime<Utc>,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: chrono::DateTime<Utc>,
}

/// Partial update for user-defined book fields.
#[derive(Debug, Clone, Default)]
pub struct UserBookFields {
    /// Operator or storefront tags (serialized string).
    pub tags: Option<String>,
    /// Overall user rating from the storefront, if any.
    pub rating_overall: Option<f32>,
    /// Narration/performance rating from the storefront, if any.
    pub rating_performance: Option<f32>,
    /// Story rating from the storefront, if any.
    pub rating_story: Option<f32>,
    /// Whether the listener marked the title finished (0/1 or bool).
    pub is_finished: Option<bool>,
}

/// Durable catalog enrichment fields (blurbs, subjects, provenance).
#[derive(Debug, Clone, Default)]
pub struct CatalogEnrichmentFields {
    /// Blurb / synopsis text (may contain HTML).
    pub description: Option<String>,
    /// BCP-47 or storefront language code when known.
    pub language: Option<String>,
    /// HTTPS URL for cover art when known.
    pub cover_url: Option<String>,
    /// Subject / topic tags from enrichment (serialized).
    pub subjects: Option<String>,
    /// Storefront category / genre path list (serialized).
    pub categories: Option<String>,
    /// Plugin or catalog that last enriched bibliographic fields.
    pub enrich_source: Option<String>,
    /// 0–1 confidence score for the last enrichment pass.
    pub enrich_confidence: Option<f64>,
    /// RFC 3339 time of the last enrichment write.
    pub enrich_updated_at: Option<chrono::DateTime<Utc>>,
}

/// One title a storefront scan reported.
#[derive(Debug, Clone)]
pub struct ScannedTitle {
    /// The flat row the library views read.
    pub book: NewBook,
    /// The listing the plugin's structured record describes; `None` when
    /// only the flat row is known.
    pub listing: Option<bookclerk_resolve::Listing>,
}

impl From<NewBook> for ScannedTitle {
    fn from(book: NewBook) -> Self {
        Self {
            book,
            listing: None,
        }
    }
}

/// Input for upserting listening progress.
#[derive(Debug, Clone)]
pub struct NewListeningProgress {
    /// Foreign key to `portal_identities.id`.
    pub identity_id: Option<i64>,
    /// External identity or integration provider id (for example ABS).
    pub provider: String,
    /// User id at the external provider.
    pub external_user_id: String,
    /// The work the item resolved to through its identifiers.
    pub work_id: String,
    /// The edition the item's identifiers name, when exactly one does.
    pub edition_id: Option<String>,
    /// Provider-native listening-progress item id.
    pub external_item_id: String,
    /// Fractional progress 0.0–1.0 when the provider reports it.
    pub progress: Option<f64>,
    /// Current playback position within the title, in seconds.
    pub current_time_seconds: Option<f64>,
    /// Total duration in seconds when known.
    pub duration_seconds: Option<f64>,
    /// Whether the listener marked the title finished (0/1 or bool).
    pub is_finished: bool,
    /// RFC 3339 time of the last playback update.
    pub last_listened_at: Option<chrono::DateTime<Utc>>,
}

/// Input for creating a title request / wishlist row.
#[derive(Debug, Clone)]
pub struct NewTitleRequest {
    /// Stable UUID for this row (API / foreign-key identity).
    pub uuid: Option<String>,
    /// Foreign key to `portal_identities.id`.
    pub identity_id: Option<i64>,
    /// Title the wisher asked for. It only finds (or makes) the wish's
    /// work and is not stored on the wish.
    pub title: String,
    /// Comma-separated authors the wisher gave, used like `title`.
    pub authors: Option<String>,
    /// Amazon ASIN the wisher gave, used like `title`.
    pub asin: Option<String>,
    /// ISBN-10/13 the wisher gave, used like `title`.
    pub isbn: Option<String>,
    /// Free-form operator or requester notes.
    pub notes: Option<String>,
    /// Lifecycle status for the row (user, request, …).
    pub status: RequestStatus,
    /// Library `books.uuid` once the wishlist item is fulfilled.
    pub resolved_book_uuid: Option<String>,
    /// HTTPS URL for cover art when known.
    pub cover_url: Option<String>,
}

impl NewTitleRequest {
    /// Returns true when the string still contains HTML character entities.
    #[must_use]
    pub fn needs_html_entity_decode(&self) -> bool {
        crate::str_maybe_html_entity(&self.title)
            || self
                .authors
                .as_deref()
                .is_some_and(crate::str_maybe_html_entity)
            || self
                .notes
                .as_deref()
                .is_some_and(crate::str_maybe_html_entity)
    }

    /// Decode HTML entities in human-readable fields.
    pub fn decode_html_entities(&mut self) {
        crate::decode_html_entities_in_place(&mut self.title);
        crate::decode_html_entities_opt_in_place(&mut self.authors);
        crate::decode_html_entities_opt_in_place(&mut self.notes);
    }
}

/// The work a wish is for, read from a store listing in the shared pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WishWork {
    /// `works.id` of the listing's resolution.
    pub work_id: String,
    /// `editions.id` the listing is an edition of.
    pub edition_id: Option<String>,
}

/// Input for upserting a per-storefront wishlist snapshot.
#[derive(Debug, Clone, Default)]
pub struct NewTitleRequestSource {
    /// Content-source plugin id (`audible`, `libro`, …).
    pub source: String,
    /// Storefront product id (ASIN, ISBN, UUID, …).
    pub product_id: String,
    /// Display title of the work or edition.
    pub title: Option<String>,
    /// Optional subtitle from bibliographic metadata.
    pub subtitle: Option<String>,
    /// Comma-separated or JSON author list from the storefront.
    pub authors: Option<String>,
    /// Comma-separated or JSON narrator list when present.
    pub narrators: Option<String>,
    /// Series name when the title belongs to a series.
    pub series: Option<String>,
    /// Position within the series (storefront string form).
    pub series_index: Option<String>,
    /// Amazon ASIN when known; otherwise null.
    pub asin: Option<String>,
    /// ISBN-10/13 when known; otherwise null.
    pub isbn: Option<String>,
    /// Blurb / synopsis text (may contain HTML).
    pub description: Option<String>,
    /// Publisher name from metadata enrichment or the storefront.
    pub publisher: Option<String>,
    /// Runtime in whole minutes when the storefront reports it.
    pub length_minutes: Option<i64>,
    /// Publication date string from the storefront or enrichment.
    pub published_at: Option<String>,
    /// Storefront category / genre path list (serialized).
    pub categories: Option<String>,
    /// BCP-47 or storefront language code when known.
    pub language: Option<String>,
    /// HTTPS URL for cover art when known.
    pub cover_url: Option<String>,
    /// Storefront product or purchase URL when known.
    pub url: Option<String>,
    /// Observed price in minor currency units, when known.
    pub price_cents: Option<i64>,
    /// ISO 4217 currency code for price fields.
    pub currency: Option<String>,
    /// Storefront-formatted price string for display.
    pub price_label: Option<String>,
    /// List/MSRP price in minor units, when known.
    pub list_price_cents: Option<i64>,
    /// Storefront-formatted list price for display.
    pub list_price_label: Option<String>,
    /// Member/subscriber price in minor units, when known.
    pub member_price_cents: Option<i64>,
    /// Storefront-formatted member price for display.
    pub member_price_label: Option<String>,
    /// The shared pool's listing for this store edition, when the server
    /// looked it up at the store ([`crate::CatalogListing`]); `None` keeps
    /// what the row already names.
    pub listing_id: Option<String>,
}

impl NewTitleRequestSource {
    /// Returns true when the string still contains HTML character entities.
    #[must_use]
    pub fn needs_html_entity_decode(&self) -> bool {
        self.title
            .as_deref()
            .is_some_and(crate::str_maybe_html_entity)
            || self
                .subtitle
                .as_deref()
                .is_some_and(crate::str_maybe_html_entity)
            || self
                .authors
                .as_deref()
                .is_some_and(crate::str_maybe_html_entity)
            || self
                .narrators
                .as_deref()
                .is_some_and(crate::str_maybe_html_entity)
            || self
                .series
                .as_deref()
                .is_some_and(crate::str_maybe_html_entity)
            || self
                .description
                .as_deref()
                .is_some_and(crate::str_maybe_html_entity)
            || self
                .publisher
                .as_deref()
                .is_some_and(crate::str_maybe_html_entity)
            || self
                .categories
                .as_deref()
                .is_some_and(crate::str_maybe_html_entity)
            || self
                .price_label
                .as_deref()
                .is_some_and(crate::str_maybe_html_entity)
            || self
                .list_price_label
                .as_deref()
                .is_some_and(crate::str_maybe_html_entity)
            || self
                .member_price_label
                .as_deref()
                .is_some_and(crate::str_maybe_html_entity)
    }

    /// Decode HTML entities in human-readable metadata fields.
    pub fn decode_html_entities(&mut self) {
        crate::decode_html_entities_opt_in_place(&mut self.title);
        crate::decode_html_entities_opt_in_place(&mut self.subtitle);
        crate::decode_html_entities_opt_in_place(&mut self.authors);
        crate::decode_html_entities_opt_in_place(&mut self.narrators);
        crate::decode_html_entities_opt_in_place(&mut self.series);
        crate::decode_html_entities_opt_in_place(&mut self.description);
        crate::decode_html_entities_opt_in_place(&mut self.publisher);
        crate::decode_html_entities_opt_in_place(&mut self.categories);
        crate::decode_html_entities_opt_in_place(&mut self.price_label);
        crate::decode_html_entities_opt_in_place(&mut self.list_price_label);
        crate::decode_html_entities_opt_in_place(&mut self.member_price_label);
    }
}

/// Local fallback when callers do not supply a discover `work_map_key`.
///
/// Converts ISBN-10 → ISBN-13 when possible so 10/13 variants share a key.
/// Soft keys here are a simple lowercase fallback — Discover re-merges with
/// richer identity matching when ranking the global queue.
#[must_use]
pub fn fallback_work_key(
    title: &str,
    authors: Option<&str>,
    asin: Option<&str>,
    isbn: Option<&str>,
) -> String {
    let mut isbn_digits: String = isbn
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == 'X' || *c == 'x')
        .collect::<String>()
        .to_ascii_uppercase();
    if isbn_digits.len() == 10 {
        let core = &isbn_digits[..9];
        if core.chars().all(|c| c.is_ascii_digit()) {
            let mut body = String::from("978");
            body.push_str(core);
            let mut sum = 0u32;
            for (i, c) in body.chars().enumerate() {
                let d = c.to_digit(10).unwrap_or(0);
                sum += if i % 2 == 0 { d } else { d * 3 };
            }
            let check = (10 - (sum % 10)) % 10;
            body.push(char::from_digit(check, 10).unwrap_or('0'));
            isbn_digits = body;
        }
    }
    if !isbn_digits.is_empty() {
        return format!("isbn:{isbn_digits}");
    }
    if let Some(asin) = asin.map(str::trim).filter(|s| !s.is_empty()) {
        return format!("asin:{}", asin.to_ascii_uppercase());
    }
    let t = title.trim().to_ascii_lowercase();
    let a = authors
        .unwrap_or("")
        .split([',', ';', '&', '/'])
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_ascii_lowercase();
    if t.is_empty() && a.is_empty() {
        return String::from("title:");
    }
    format!("soft:{t}|{a}")
}

// ── Entity → record mapping ─────────────────────────────────────────────────

/// Current UTC timestamp as RFC 3339 text for SeaORM `TEXT` columns.
fn now_str() -> String {
    Utc::now().to_rfc3339()
}

/// True when `incoming` is the same OIDC owner as `stored`.
///
/// Exact match, or an alias-era row (`audiobookshelf`) rewritten to that
/// plugin's PluginKey (`…#audiobookshelf`). A stored PluginKey never matches
/// a different key through the alias fragment.
fn oidc_plugin_owner_matches(stored: &str, incoming: &str) -> bool {
    if stored == incoming {
        return true;
    }
    let Some((_, alias)) = incoming.rsplit_once('#') else {
        return false;
    };
    !stored.contains(':') && stored == alias
}

/// Default OpenID scopes granted to a new Bookclerk-as-IdP client.
fn default_oidc_scopes() -> Vec<String> {
    vec!["openid".into(), "profile".into(), "email".into()]
}

/// Keep known OpenID scopes in a stable order, dropping empties.
fn normalize_oidc_scopes(scopes: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for wanted in ["openid", "profile", "email"] {
        if scopes.iter().any(|s| s.trim().eq_ignore_ascii_case(wanted)) {
            out.push(wanted.to_string());
        }
    }
    for scope in scopes {
        let trimmed = scope.trim();
        if trimmed.is_empty() {
            continue;
        }
        if out.iter().any(|s| s.eq_ignore_ascii_case(trimmed)) {
            continue;
        }
        out.push(trimmed.to_string());
    }
    if out.is_empty() {
        default_oidc_scopes()
    } else {
        out
    }
}

/// Map an `oidc_clients` row onto [`OidcClientRecord`].
fn oidc_client_from_model(model: oidc_clients::Model) -> OidcClientRecord {
    let uris: Vec<String> = serde_json::from_str(&model.redirect_uris_json).unwrap_or_default();
    let scopes: Vec<String> = serde_json::from_str(&model.allowed_scopes_json).unwrap_or_default();
    OidcClientRecord {
        id: model.id,
        client_id: model.client_id,
        client_secret_hash: model.client_secret_hash,
        redirect_uris: uris,
        name: model.name,
        issue_refresh_token: model.issue_refresh_token != 0,
        allowed_scopes: if scopes.is_empty() {
            default_oidc_scopes()
        } else {
            scopes
        },
        enabled: model.enabled != 0,
        plugin_id: model.plugin_id.and_then(|id| {
            let trimmed = id.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }),
        created_at: model.created_at,
    }
}

/// Trim + lowercase login; empty/whitespace becomes `None` (no login).
fn normalize_login_name(login_name: Option<&str>) -> Option<String> {
    login_name.and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_ascii_lowercase())
        }
    })
}

/// Minimum age of `portal_sessions.last_used_at` before an authenticated
/// lookup rewrites it. Keeps presence idle/active meaningful without a write
/// on every request.
const PORTAL_SESSION_LAST_USED_MIN_INTERVAL: chrono::Duration = chrono::Duration::seconds(60);

/// Parses an RFC 3339 timestamp as UTC; unparseable values fall back to now.
fn parse_dt(value: &str) -> chrono::DateTime<Utc> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

/// Parses an optional RFC 3339 timestamp; `None` stays `None`.
fn parse_dt_opt(value: Option<&str>) -> Option<chrono::DateTime<Utc>> {
    value.map(parse_dt)
}

/// Conflict error naming every plugin that owns a matching account.
fn ambiguous_account(needle: &str, rows: &[AccountRecord]) -> LibraryError {
    let owners: Vec<String> = rows
        .iter()
        .map(|a| format!("{} ({})", a.source, a.plugin_key))
        .collect();
    LibraryError::Conflict(format!(
        "account `{needle}` matches more than one plugin ({}); pass --source",
        owners.join(", ")
    ))
}

/// Maps an `accounts` row to [`AccountRecord`] (`scan_enabled` is stored as 0/1).
fn map_account(m: accounts::Model) -> AccountRecord {
    AccountRecord {
        id: m.id,
        plugin_key: m.plugin_key,
        account_id: m.account_id,
        source: m.source,
        marketplace: m.marketplace,
        label: m.label,
        scan_enabled: m.scan_enabled != 0,
        connection_status: m.connection_status,
        created_at: parse_dt(&m.created_at),
        updated_at: parse_dt(&m.updated_at),
    }
}

/// Maps a `portal_identities` row to the public portal-identity record.
fn map_portal_identity(m: portal_identities::Model) -> crate::models::PortalIdentity {
    crate::models::PortalIdentity {
        id: m.id,
        provider: m.provider,
        external_user_id: m.external_user_id,
        label: m.label,
        user_id: m.user_id,
        created_at: parse_dt(&m.created_at),
        picture_url: m.picture_url,
    }
}

/// Builds a queue-avatar row for one open wishlist request.
///
/// # Arguments
///
/// * `identity_id` - Portal identity on the title-request row, if any.
/// * `identities` - Identities loaded for this queue pass.
/// * `users` - First-party users linked from those identities.
///
/// # Returns
///
/// Operator placeholder when `identity_id` is none; otherwise the linked user
/// or unlinked identity label.
fn queue_wisher_for(
    identity_id: Option<i64>,
    identities: &std::collections::HashMap<i64, PortalIdentity>,
    users: &std::collections::HashMap<i64, UserRecord>,
) -> QueueWisher {
    let Some(identity_id) = identity_id else {
        return QueueWisher {
            display_name: Some(String::from("Operator")),
            operator: true,
            ..QueueWisher::default()
        };
    };
    let ident = identities.get(&identity_id);
    let user = ident
        .and_then(|i| i.user_id)
        .and_then(|uid| users.get(&uid));
    let picture_url = ident
        .and_then(|i| i.picture_url.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    QueueWisher {
        user_id: user.map(|u| u.id),
        identity_id: Some(identity_id),
        display_name: user
            .and_then(|u| u.display_name.clone())
            .filter(|s| !s.trim().is_empty())
            .or_else(|| ident.and_then(|i| i.label.clone())),
        login_name: user.and_then(|u| u.login_name.clone()),
        operator: false,
        has_avatar: false,
        avatar_source: user.and_then(|u| u.avatar_source.clone()),
        gravatar_hash: user
            .and_then(|u| u.email.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(crate::gravatar_hash),
        picture_url,
        picture_path: None,
    }
}

/// Maps a `users` row to [`UserRecord`]; unknown role/status fall back to member/active.
fn map_user(m: users::Model) -> UserRecord {
    UserRecord {
        id: m.id,
        role: UserRole::parse(&m.role).unwrap_or(UserRole::Member),
        status: UserStatus::parse(&m.status).unwrap_or(UserStatus::Active),
        display_name: m.display_name,
        login_name: m.login_name,
        email: m.email,
        has_password: m.password_hash.is_some(),
        security_version: m.security_version,
        created_at: parse_dt(&m.created_at),
        updated_at: parse_dt(&m.updated_at),
        last_seen_at: parse_dt_opt(m.last_seen_at.as_deref()),
        avatar_source: m.avatar_source,
        totp_enabled: m.totp_enabled != 0,
    }
}

/// Maps a `user_invites` row to the public invite record (token is stored as a hash).
fn map_user_invite(m: user_invites::Model) -> crate::models::UserInviteRecord {
    crate::models::UserInviteRecord {
        id: m.id,
        token_hash: m.token_hash,
        role: UserRole::parse(&m.role).unwrap_or(UserRole::Member),
        login_name: m.login_name,
        display_name: m.display_name,
        expires_at: parse_dt(&m.expires_at),
        redeemed_at: parse_dt_opt(m.redeemed_at.as_deref()),
        created_by: m.created_by,
        created_at: parse_dt(&m.created_at),
    }
}

/// Maps a `claim_tickets` row to the public ticket record (token is stored as a hash).
fn map_claim_ticket(m: claim_tickets::Model) -> crate::models::ClaimTicketRecord {
    crate::models::ClaimTicketRecord {
        id: m.id,
        token_hash: m.token_hash,
        identity_id: m.identity_id,
        expires_at: parse_dt(&m.expires_at),
        redeemed_at: parse_dt_opt(m.redeemed_at.as_deref()),
        created_by: m.created_by,
        created_at: parse_dt(&m.created_at),
    }
}

/// Maps an `account_links` row (portal identity ↔ store account) to the public record.
fn map_account_link(m: account_links::Model) -> crate::models::AccountLinkRecord {
    crate::models::AccountLinkRecord {
        id: m.id,
        identity_id: m.identity_id,
        plugin_key: m.plugin_key,
        account_id: m.account_id,
        source: m.source,
        created_at: parse_dt(&m.created_at),
    }
}

/// Maps a `user_preferences` row, parsing JSON shelf/source lists and normalizing Discover sort.
fn map_user_preferences(m: user_preferences::Model) -> UserPreferences {
    let disabled_shelves: Vec<String> =
        serde_json::from_str(&m.disabled_shelves_json).unwrap_or_default();
    let discover_excluded_sources: Vec<String> =
        serde_json::from_str(&m.discover_excluded_sources_json).unwrap_or_default();
    UserPreferences {
        id: m.id,
        subject_key: m.subject_key,
        identity_id: m.identity_id,
        default_view: m.default_view,
        disabled_shelves,
        discover_sort: normalize_discover_sort(&m.discover_sort),
        discover_sort_dir: normalize_discover_sort_dir(&m.discover_sort_dir),
        discover_language: m.discover_language.filter(|s| !s.trim().is_empty()),
        discover_excluded_sources,
        theme: crate::normalize_theme(&m.theme),
        updated_at: parse_dt(&m.updated_at),
    }
}

/// Canonical Discover sort key; unknown values become `relevance`.
fn normalize_discover_sort(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "popularity" => String::from("popularity"),
        "rating" => String::from("rating"),
        "title" => String::from("title"),
        "author" => String::from("author"),
        "price" => String::from("price"),
        "length" | "runtime" => String::from("length"),
        _ => String::from("relevance"),
    }
}

/// Canonical Discover sort direction; anything other than `asc`/`ascending` is `desc`.
fn normalize_discover_sort_dir(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "asc" | "ascending" => String::from("asc"),
        _ => String::from("desc"),
    }
}

/// Maps a `saved_filters` row to the public saved-filter record.
fn map_saved_filter(m: saved_filters::Model) -> SavedFilterRecord {
    SavedFilterRecord {
        id: m.id,
        name: m.name,
        query: m.query,
        created_at: parse_dt(&m.created_at),
        updated_at: parse_dt(&m.updated_at),
    }
}

/// Maps a `listening_progress` row; `is_finished` is stored as 0/1.
fn map_listening(m: listening_progress::Model) -> ListeningProgressRecord {
    ListeningProgressRecord {
        id: m.id,
        identity_id: m.identity_id,
        provider: m.provider,
        external_user_id: m.external_user_id,
        work_id: m.work_id,
        edition_id: m.edition_id,
        external_item_id: m.external_item_id,
        progress: m.progress,
        current_time_seconds: m.current_time_seconds,
        duration_seconds: m.duration_seconds,
        is_finished: m.is_finished != 0,
        last_listened_at: parse_dt_opt(m.last_listened_at.as_deref()),
        updated_at: parse_dt(&m.updated_at),
    }
}

/// Maps a `title_requests` row; [`LibraryStore::hydrate_requests`] fills the
/// work's title and authors, the sources and the card key.
fn map_request(m: title_requests::Model) -> TitleRequestRecord {
    TitleRequestRecord {
        cover_path: None,
        id: m.id,
        uuid: m.uuid,
        identity_id: m.identity_id,
        title: String::new(),
        authors: None,
        asin: None,
        isbn: None,
        notes: m.notes,
        status: RequestStatus::parse(&m.status).unwrap_or_default(),
        work_key: String::new(),
        work_id: m.work_id,
        edition_id: m.edition_id,
        resolved_book_uuid: m.resolved_book_uuid,
        cover_url: m.cover_url,
        sources: Vec::new(),
        description: None,
        subtitle: None,
        narrators: None,
        series: None,
        series_index: None,
        publisher: None,
        length_minutes: None,
        published_at: None,
        genres: None,
        language: None,
        store_editions: Vec::new(),
        purchase_hints: Vec::new(),
        created_at: parse_dt(&m.created_at),
        updated_at: parse_dt(&m.updated_at),
    }
}

/// Maps a `title_request_sources` row including observed list/member prices in cents.
fn map_source(m: title_request_sources::Model) -> TitleRequestSourceRecord {
    TitleRequestSourceRecord {
        id: m.id,
        title_request_id: m.title_request_id,
        source: m.source,
        product_id: m.product_id,
        title: m.title,
        subtitle: m.subtitle,
        authors: m.authors,
        narrators: m.narrators,
        series: m.series,
        series_index: m.series_index,
        asin: m.asin,
        isbn: m.isbn,
        description: m.description,
        publisher: m.publisher,
        length_minutes: m.length_minutes,
        published_at: m.published_at,
        categories: m.categories,
        language: m.language,
        cover_url: m.cover_url,
        url: m.url,
        price_cents: m.price_cents,
        currency: m.currency,
        price_label: m.price_label,
        list_price_cents: m.list_price_cents,
        list_price_label: m.list_price_label,
        member_price_cents: m.member_price_cents,
        member_price_label: m.member_price_label,
        observed_at: parse_dt_opt(m.observed_at.as_deref()),
        created_at: parse_dt(&m.created_at),
        updated_at: parse_dt(&m.updated_at),
    }
}

/// Trims an optional string; empty or whitespace becomes `None`.
fn trim_opt(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// Prefers a non-empty incoming string over the current optional value.
fn prefer_opt(current: Option<String>, incoming: Option<&str>) -> Option<String> {
    let next = trim_opt(incoming);
    match (current, next) {
        (None, n) => n,
        (c, None) => c,
        (Some(c), Some(n)) => Some(n).filter(|s| !s.is_empty()).or(Some(c)),
    }
}

/// Builds an ActiveModel that merges incoming storefront fields onto an existing source row.
fn merged_source_active(
    model: title_request_sources::Model,
    src: &NewTitleRequestSource,
    now: &str,
) -> title_request_sources::ActiveModel {
    let id = model.id;
    let title_request_id = model.title_request_id;
    let source = model.source.clone();
    let product_id = model.product_id.clone();
    let created_at = model.created_at.clone();
    title_request_sources::ActiveModel {
        id: Set(id),
        title_request_id: Set(title_request_id),
        source: Set(source),
        product_id: Set(product_id),
        title: Set(prefer_opt(model.title, src.title.as_deref())),
        subtitle: Set(prefer_opt(model.subtitle, src.subtitle.as_deref())),
        authors: Set(prefer_opt(model.authors, src.authors.as_deref())),
        narrators: Set(prefer_opt(model.narrators, src.narrators.as_deref())),
        series: Set(prefer_opt(model.series, src.series.as_deref())),
        series_index: Set(prefer_opt(model.series_index, src.series_index.as_deref())),
        asin: Set(prefer_opt(model.asin, src.asin.as_deref())),
        isbn: Set(prefer_opt(model.isbn, src.isbn.as_deref())),
        description: Set(crate::wishlist_merge::pick_better_description(
            model.description.as_deref(),
            src.description.as_deref(),
        )),
        publisher: Set(prefer_opt(model.publisher, src.publisher.as_deref())),
        length_minutes: Set(src.length_minutes.or(model.length_minutes)),
        published_at: Set(prefer_opt(model.published_at, src.published_at.as_deref())),
        categories: Set(prefer_opt(model.categories, src.categories.as_deref())),
        language: Set(prefer_opt(model.language, src.language.as_deref())),
        cover_url: Set(prefer_opt(model.cover_url, src.cover_url.as_deref())),
        url: Set(prefer_opt(model.url, src.url.as_deref())),
        price_cents: Set(src.price_cents.or(model.price_cents)),
        currency: Set(prefer_opt(model.currency, src.currency.as_deref())),
        price_label: Set(prefer_opt(model.price_label, src.price_label.as_deref())),
        list_price_cents: Set(src.list_price_cents.or(model.list_price_cents)),
        list_price_label: Set(prefer_opt(
            model.list_price_label,
            src.list_price_label.as_deref(),
        )),
        member_price_cents: Set(src.member_price_cents.or(model.member_price_cents)),
        member_price_label: Set(prefer_opt(
            model.member_price_label,
            src.member_price_label.as_deref(),
        )),
        observed_at: Set(Some(now.to_string())),
        listing_id: Set(src.listing_id.clone().or(model.listing_id)),
        created_at: Set(created_at),
        updated_at: Set(now.to_string()),
    }
}

/// Maps a `books` row to [`BookRecord`], parsing acquire/pdf status and RFC 3339 timestamps.
fn map_book(m: books::Model) -> Result<BookRecord> {
    Ok(BookRecord {
        id: m.id,
        uuid: m.uuid,
        plugin_key: m.plugin_key,
        source: m.source,
        account_id: m.account_id,
        product_id: m.product_id,
        asin: m.asin,
        isbn: m.isbn,
        marketplace: m.marketplace,
        title: m.title,
        authors: m.authors,
        narrators: m.narrators,
        series: m.series,
        series_index: m.series_index,
        series_asin: m.series_asin,
        acquire_status: AcquireStatus::parse(&m.acquire_status).unwrap_or_default(),
        storage_key: m.storage_key,
        error_message: m.error_message,
        purchased_at: parse_dt_opt(m.purchased_at.as_deref()),
        tags: m.tags,
        rating_overall: m.rating_overall.map(|v| v as f32),
        rating_performance: m.rating_performance.map(|v| v as f32),
        rating_story: m.rating_story.map(|v| v as f32),
        is_finished: m.is_finished != 0,
        pdf_status: AcquireStatus::parse(&m.pdf_status).unwrap_or_default(),
        pdf_storage_key: m.pdf_storage_key,
        publisher: m.publisher,
        length_minutes: m.length_minutes,
        is_abridged: m.is_abridged != 0,
        content_kind: m.content_kind,
        categories: m.categories,
        subtitle: m.subtitle,
        published_at: parse_dt_opt(m.published_at.as_deref()),
        description: m.description,
        language: m.language,
        cover_url: m.cover_url,
        cover_path: None,
        subjects: m.subjects,
        enrich_source: m.enrich_source,
        enrich_confidence: m.enrich_confidence,
        enrich_updated_at: parse_dt_opt(m.enrich_updated_at.as_deref()),
        created_at: parse_dt(&m.created_at),
        updated_at: parse_dt(&m.updated_at),
    })
}

/// A flat book row's natural key: `UNIQUE(plugin_key, account_id, product_id)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BookKey {
    /// Plugin that owns the row.
    plugin_key: String,
    /// Store account the title is in.
    account_id: String,
    /// The store's product id.
    product_id: String,
}

impl BookKey {
    /// The key `book` is stored under.
    fn of(book: &NewBook) -> Self {
        Self {
            plugin_key: book.plugin_key.clone(),
            account_id: book.account_id.clone(),
            product_id: book.product_id.clone(),
        }
    }
}

/// `book` with HTML entities decoded, cloned only when it carries any so
/// hot scan paths stay allocation-light.
fn decoded_book(book: &NewBook) -> Cow<'_, NewBook> {
    if book.needs_html_entity_decode() {
        let mut decoded = book.clone();
        decoded.decode_html_entities();
        Cow::Owned(decoded)
    } else {
        Cow::Borrowed(book)
    }
}

/// Refuses a book with no owning plugin.
///
/// # Errors
///
/// Returns an error when `book.plugin_key` is empty.
fn require_plugin_key(book: &NewBook) -> Result<()> {
    if book.plugin_key.is_empty() {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "book `{}` needs an owning plugin key",
            book.product_id
        )));
    }
    Ok(())
}

/// The stored row `existing` becomes when a scan lists `book` again, with
/// Audible-precedence rules: a Libro rescan without an ASIN must not wipe
/// Audible enrichment.
///
/// The incoming row is authoritative when it carries an Audible ASIN, or
/// the stored row never had one. Acquire state, ratings, tags and
/// enrichment are kept as stored.
fn merged_book_row(existing: &books::Model, book: &NewBook, now: &str) -> books::Model {
    let prefer_new = book.asin.is_some() || existing.asin.is_none();
    let keep = |new: &Option<String>, old: &Option<String>| {
        if prefer_new {
            new.clone()
        } else {
            old.clone()
        }
    };
    let coalesce_keep = |new: &Option<String>, old: &Option<String>| {
        if prefer_new {
            new.clone().or_else(|| old.clone())
        } else {
            old.clone()
        }
    };
    let mut row = existing.clone();
    row.source.clone_from(&book.source);
    row.asin = book.asin.clone().or_else(|| existing.asin.clone());
    row.isbn = book.isbn.clone().or_else(|| existing.isbn.clone());
    row.marketplace.clone_from(&book.marketplace);
    if prefer_new {
        row.title.clone_from(&book.title);
    }
    row.authors = keep(&book.authors, &existing.authors);
    row.narrators = keep(&book.narrators, &existing.narrators);
    row.series = keep(&book.series, &existing.series);
    row.series_index = keep(&book.series_index, &existing.series_index);
    row.series_asin = coalesce_keep(&book.series_asin, &existing.series_asin);
    row.purchased_at = book.purchased_at.map(|d| d.to_rfc3339());
    row.publisher = coalesce_keep(&book.publisher, &existing.publisher);
    if prefer_new {
        row.length_minutes = book.length_minutes.or(existing.length_minutes);
    }
    row.is_abridged = i64::from(book.is_abridged);
    row.content_kind.clone_from(&book.content_kind);
    row.categories = coalesce_keep(&book.categories, &existing.categories);
    row.subtitle = coalesce_keep(&book.subtitle, &existing.subtitle);
    if prefer_new {
        row.published_at = book
            .published_at
            .map(|d| d.to_rfc3339())
            .or_else(|| existing.published_at.clone());
    }
    now.clone_into(&mut row.updated_at);
    row
}

/// The update that writes a scan's merge (`merged`, from
/// [`merged_book_row`]) over the loaded row `existing`: only the columns a
/// scan owns are set, so acquire state, ratings, tags and enrichment that
/// another writer changed since the row was read are left alone, as
/// [`update_book_statement`] leaves them.
fn scan_update(existing: books::Model, merged: books::Model) -> books::ActiveModel {
    let mut am: books::ActiveModel = existing.into();
    am.source = Set(merged.source);
    am.asin = Set(merged.asin);
    am.isbn = Set(merged.isbn);
    am.marketplace = Set(merged.marketplace);
    am.title = Set(merged.title);
    am.authors = Set(merged.authors);
    am.narrators = Set(merged.narrators);
    am.series = Set(merged.series);
    am.series_index = Set(merged.series_index);
    am.series_asin = Set(merged.series_asin);
    am.purchased_at = Set(merged.purchased_at);
    am.publisher = Set(merged.publisher);
    am.length_minutes = Set(merged.length_minutes);
    am.is_abridged = Set(merged.is_abridged);
    am.content_kind = Set(merged.content_kind);
    am.categories = Set(merged.categories);
    am.subtitle = Set(merged.subtitle);
    am.published_at = Set(merged.published_at);
    am.updated_at = Set(merged.updated_at);
    am
}

/// The row a first sight of `book` stores; `id` is `0` until the database
/// assigns one.
fn new_book_row(book: &NewBook, now: &str) -> books::Model {
    books::Model {
        id: 0,
        uuid: book
            .uuid
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string()),
        plugin_key: book.plugin_key.clone(),
        source: book.source.clone(),
        account_id: book.account_id.clone(),
        product_id: book.product_id.clone(),
        asin: book.asin.clone(),
        isbn: book.isbn.clone(),
        marketplace: book.marketplace.clone(),
        title: book.title.clone(),
        authors: book.authors.clone(),
        narrators: book.narrators.clone(),
        series: book.series.clone(),
        series_index: book.series_index.clone(),
        series_asin: book.series_asin.clone(),
        acquire_status: AcquireStatus::NotAcquired.as_str().to_string(),
        storage_key: None,
        error_message: None,
        purchased_at: book.purchased_at.map(|d| d.to_rfc3339()),
        tags: None,
        rating_overall: None,
        rating_performance: None,
        rating_story: None,
        is_finished: 0,
        pdf_status: AcquireStatus::NotAcquired.as_str().to_string(),
        pdf_storage_key: None,
        publisher: book.publisher.clone(),
        length_minutes: book.length_minutes,
        is_abridged: i64::from(book.is_abridged),
        content_kind: book.content_kind.clone(),
        categories: book.categories.clone(),
        subtitle: book.subtitle.clone(),
        published_at: book.published_at.map(|d| d.to_rfc3339()),
        description: None,
        language: None,
        cover_url: None,
        subjects: None,
        enrich_source: None,
        enrich_confidence: None,
        enrich_updated_at: None,
        created_at: now.to_owned(),
        updated_at: now.to_owned(),
    }
}

/// The statement that writes a scan's merge ([`merged_book_row`]) over the
/// stored row with `row`'s key.
fn update_book_statement(row: &books::Model) -> bookclerk_plugin_abi::TypedDbStatement {
    batch::exec(
        "UPDATE books SET source = ?, asin = ?, isbn = ?, marketplace = ?, title = ?, \
         authors = ?, narrators = ?, series = ?, series_index = ?, series_asin = ?, \
         purchased_at = ?, publisher = ?, length_minutes = ?, is_abridged = ?, \
         content_kind = ?, categories = ?, subtitle = ?, published_at = ?, updated_at = ? \
         WHERE plugin_key = ? AND account_id = ? AND product_id = ?",
        vec![
            batch::text(&row.source),
            batch::text_opt(row.asin.as_deref()),
            batch::text_opt(row.isbn.as_deref()),
            batch::text(&row.marketplace),
            batch::text(&row.title),
            batch::text_opt(row.authors.as_deref()),
            batch::text_opt(row.narrators.as_deref()),
            batch::text_opt(row.series.as_deref()),
            batch::text_opt(row.series_index.as_deref()),
            batch::text_opt(row.series_asin.as_deref()),
            batch::text_opt(row.purchased_at.as_deref()),
            batch::text_opt(row.publisher.as_deref()),
            batch::int_opt(row.length_minutes),
            batch::int(row.is_abridged),
            batch::text(&row.content_kind),
            batch::text_opt(row.categories.as_deref()),
            batch::text_opt(row.subtitle.as_deref()),
            batch::text_opt(row.published_at.as_deref()),
            batch::text(&row.updated_at),
            batch::text(&row.plugin_key),
            batch::text(&row.account_id),
            batch::text(&row.product_id),
        ],
    )
}

/// The statement that stores a first sight ([`new_book_row`]) unless a row
/// with its key or uuid already exists.
fn insert_book_statement(row: &books::Model) -> bookclerk_plugin_abi::TypedDbStatement {
    batch::exec(
        "INSERT OR IGNORE INTO books (uuid, plugin_key, source, account_id, product_id, \
         asin, isbn, marketplace, title, authors, narrators, series, series_index, \
         series_asin, acquire_status, purchased_at, is_finished, pdf_status, publisher, \
         length_minutes, is_abridged, content_kind, categories, subtitle, published_at, \
         created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        vec![
            batch::text(&row.uuid),
            batch::text(&row.plugin_key),
            batch::text(&row.source),
            batch::text(&row.account_id),
            batch::text(&row.product_id),
            batch::text_opt(row.asin.as_deref()),
            batch::text_opt(row.isbn.as_deref()),
            batch::text(&row.marketplace),
            batch::text(&row.title),
            batch::text_opt(row.authors.as_deref()),
            batch::text_opt(row.narrators.as_deref()),
            batch::text_opt(row.series.as_deref()),
            batch::text_opt(row.series_index.as_deref()),
            batch::text_opt(row.series_asin.as_deref()),
            batch::text(&row.acquire_status),
            batch::text_opt(row.purchased_at.as_deref()),
            batch::int(row.is_finished),
            batch::text(&row.pdf_status),
            batch::text_opt(row.publisher.as_deref()),
            batch::int_opt(row.length_minutes),
            batch::int(row.is_abridged),
            batch::text(&row.content_kind),
            batch::text_opt(row.categories.as_deref()),
            batch::text_opt(row.subtitle.as_deref()),
            batch::text_opt(row.published_at.as_deref()),
            batch::text(&row.created_at),
            batch::text(&row.updated_at),
        ],
    )
}

/// Maps a SeaORM engine failure onto a structured guest [`bookclerk_plugin_abi::PluginError`].
fn plugin_err_from_db(err: sea_orm::DbErr) -> bookclerk_plugin_abi::PluginError {
    let msg = err.to_string();
    if let Some(limit) = bookclerk_plugin_abi::LimitExceeded::from_message(&msg) {
        let mut typed = bookclerk_plugin_abi::PluginError::new(
            bookclerk_plugin_abi::PluginErrorCode::LimitExceeded,
            msg,
        );
        typed.details = Some(limit.details());
        typed
    } else if msg.contains("cancelled:") {
        bookclerk_plugin_abi::PluginError::cancelled(msg)
    } else if msg.contains("deadline_exceeded") {
        bookclerk_plugin_abi::PluginError::deadline_exceeded(msg)
    } else if msg.contains("different requestHash")
        || msg.contains("different request")
        || msg.contains("requestHash does not match")
    {
        bookclerk_plugin_abi::PluginError::conflict(msg)
    } else {
        bookclerk_plugin_abi::PluginError::internal(msg)
    }
}

impl LibraryStore {
    /// Runs a host-authored canonical SQL batch as one transaction.
    ///
    /// When a guest [`crate::TypedAtomicExec`] is attached, the stamped envelope
    /// is sent there. Otherwise the batch runs on this connection.
    ///
    /// # Errors
    ///
    /// Returns an error when typecheck, transport, or the engine rejects the batch.
    pub(crate) async fn execute_host_batch(
        &self,
        req: bookclerk_plugin_abi::ExecuteRequest,
    ) -> Result<bookclerk_plugin_abi::ExecuteReply> {
        self.execute_host_batch_limited(req, 8).await
    }

    /// [`Self::execute_host_batch`] with an in-process row cap.
    ///
    /// Attached guests keep their negotiated result cap. `max_result_rows`
    /// bounds the in-process fallback (`0` is unlimited).
    ///
    /// # Errors
    ///
    /// Returns an error when typecheck, transport, or the engine rejects the batch.
    pub(crate) async fn execute_host_batch_limited(
        &self,
        req: bookclerk_plugin_abi::ExecuteRequest,
        max_result_rows: u32,
    ) -> Result<bookclerk_plugin_abi::ExecuteReply> {
        let type_env = crate::migrations::host_sql_type_env();
        let envelope = bookclerk_db_exec::stamp_adapter_execute(req.clone(), &type_env)
            .map_err(LibraryError::from_db_err)?;
        if let Some(exec) = &self.typed_exec {
            return exec.execute_typed(envelope).await.map_err(|err| {
                let message = err.to_string();
                if message.contains("unavailable") || message.contains("ambiguous") {
                    LibraryError::Unavailable(message)
                } else {
                    LibraryError::Other(anyhow::anyhow!(message))
                }
            });
        }
        crate::sql_plan::execute_typed_on(&self.db, &req, max_result_rows).await
    }
}

pub(crate) mod catalog_listings;
pub(crate) mod discover;
pub(crate) mod edition_files;
pub(crate) mod event_outbox;
pub(crate) mod library_model;
pub(crate) mod listening;
pub(crate) mod listing_reads;
pub(crate) mod plugin_databases;
pub(crate) mod plugin_kv;
pub(crate) mod request_queue;
pub(crate) mod retention;
pub(crate) mod scan_state;
pub(crate) mod search_feed;
pub(crate) mod storage_scan;
pub(crate) mod user_title_state;
pub(crate) mod write_batches;
pub use discover::{
    CandidateRef, DiscoverSubject, SeedScope, StoreCallHit, StoredCandidate, StoredFeed,
};
pub use edition_files::{EditionFile, FileOwner, FileRole, ACQUIRE_WRITER};
pub use event_outbox::{inject_dispatch_page_failures, set_dispatch_chunk_for_test};
pub use request_queue::{QueueRank, StoredQueueRank, MAX_QUEUE_REASONS};
pub(crate) mod job_queue;

mod book_lookup;
mod books_page;
mod lock_retry;
pub use book_lookup::{AcquiredBookKey, ACQUIRED_KEYS_MAX_PAGE};
pub use books_page::{BookPage, BOOK_PAGE_DEFAULT_LIMIT, BOOK_PAGE_MAX_LIMIT};

#[cfg(test)]
mod paging_tests;
#[cfg(test)]
mod tests;
