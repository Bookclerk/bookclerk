//! SeaORM-backed, async library store.
//!
//! [`LibraryStore`] holds a [`DatabaseConnection`] opened by the database plugin
//! (SQLite / D1 / Postgres guest) and drives the majority of CRUD through the
//! typed SeaORM [`entities`](crate::entities) (`Entity::find`, `ActiveModel`,
//! `QueryFilter`). A handful of upserts with COALESCE/precedence semantics are
//! expressed as load-then-merge over the same entities so they stay
//! backend-portable. Timestamps live in the DB as RFC 3339 `TEXT`; records
//! expose `chrono::DateTime<Utc>` and conversions happen at the record boundary.

use chrono::Utc;
use sea_orm::{
    ActiveModelTrait,
    ActiveValue::{NotSet, Set},
    ColumnTrait, Condition, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect,
};
use uuid::Uuid;

use crate::entities::{
    account_links, accounts, books, claim_tickets, embeddings, ignored_titles, listening_progress,
    oidc_auth_codes, oidc_clients, oidc_refresh_tokens, operator_sessions, portal_identities,
    portal_sessions, saved_filters, security_audit_events, title_request_sources, title_requests,
    user_invites, user_preferences, users, work_editions, works,
};
use crate::error::{LibraryError, Result};
use crate::models::{
    AccountRecord, AcquireStatus, BookRecord, GlobalQueueEntry, ListeningProgressRecord,
    RequestStatus, TitleRequestRecord, TitleRequestSourceRecord, UserPreferences, UserRecord,
    UserRole, UserStatus, WorkRecord,
};
use crate::wishlist_merge::apply_merged_sources;

/// Handle to the Bookclerk library database.
///
/// Async API over a SeaORM [`DatabaseConnection`]. `DatabaseConnection` is
/// cheaply cloneable (shared connection), so `LibraryStore` is `Clone`.
#[derive(Clone)]
pub struct LibraryStore {
    db: DatabaseConnection,
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
        Self { db }
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

    /// Upsert an account (updates `scan_enabled` on conflict).
    ///
    /// `source` is required — there is no audible default. Content sources should
    /// prefer [`crate::SourceScope::upsert_account`], which forces the plugin id.
    pub async fn upsert_account(
        &self,
        account_id: &str,
        marketplace: &str,
        label: Option<&str>,
        scan_enabled: bool,
        source: &str,
    ) -> Result<AccountRecord> {
        self.upsert_account_inner(account_id, marketplace, label, scan_enabled, source, true)
            .await
    }

    /// Ensure an account row exists without overwriting `scan_enabled`.
    ///
    /// `source` is required — there is no audible default. Content sources should
    /// prefer [`crate::SourceScope::ensure_account`].
    pub async fn ensure_account(
        &self,
        account_id: &str,
        marketplace: &str,
        label: Option<&str>,
        source: &str,
    ) -> Result<AccountRecord> {
        self.upsert_account_inner(account_id, marketplace, label, true, source, false)
            .await
    }

    async fn upsert_account_inner(
        &self,
        account_id: &str,
        marketplace: &str,
        label: Option<&str>,
        scan_enabled: bool,
        source: &str,
        update_scan_enabled: bool,
    ) -> Result<AccountRecord> {
        let now = now_str();
        let existing = accounts::Entity::find()
            .filter(accounts::Column::AccountId.eq(account_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;

        if let Some(model) = existing {
            if model.source != source {
                return Err(LibraryError::Other(anyhow::anyhow!(
                    "account_id `{account_id}` already exists for source `{}`; \
                     cannot claim it for source `{source}`",
                    model.source
                )));
            }
            let mut am: accounts::ActiveModel = model.into();
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

    /// Remap `from` → `to` account ids (books + account row).
    ///
    /// Used when classic Libation stored an email `AccountId` and a later
    /// login/scan discovers Audible `customer_id`.
    pub async fn remap_account_id(&self, from: &str, to: &str) -> Result<()> {
        if from == to {
            return Ok(());
        }
        let Some(from_acct) = accounts::Entity::find()
            .filter(accounts::Column::AccountId.eq(from))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(());
        };

        let to_acct = accounts::Entity::find()
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

        // Move books that do not already exist under `to` (same source + product_id).
        let existing_keys: std::collections::HashSet<(String, String)> = books::Entity::find()
            .filter(books::Column::AccountId.eq(to))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(|b| (b.source, b.product_id))
            .collect();

        let from_books = books::Entity::find()
            .filter(books::Column::AccountId.eq(from))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        for book in from_books {
            if existing_keys.contains(&(book.source.clone(), book.product_id.clone())) {
                continue;
            }
            let mut am: books::ActiveModel = book.into();
            am.account_id = Set(to.to_string());
            am.updated_at = Set(now.clone());
            am.update(&self.db).await.map_err(LibraryError::Orm)?;
        }

        // Drop duplicate product rows left on the old id, then the account.
        books::Entity::delete_many()
            .filter(books::Column::AccountId.eq(from))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        accounts::Entity::delete_many()
            .filter(accounts::Column::AccountId.eq(from))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Remap any existing alias account ids onto `canonical_id`, then upsert.
    pub async fn reconcile_account_id(
        &self,
        canonical_id: &str,
        aliases: &[&str],
        marketplace: &str,
        label: Option<&str>,
        scan_enabled: bool,
        source: &str,
    ) -> Result<AccountRecord> {
        for alias in aliases {
            if *alias != canonical_id {
                self.remap_account_id(alias, canonical_id).await?;
            }
        }
        self.upsert_account(canonical_id, marketplace, label, scan_enabled, source)
            .await
    }

    pub async fn get_account(&self, account_id: &str) -> Result<Option<AccountRecord>> {
        Ok(accounts::Entity::find()
            .filter(accounts::Column::AccountId.eq(account_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_account))
    }

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

    /// Count account rows (SQL `COUNT`, not a full fetch).
    pub async fn count_accounts(&self) -> Result<i64> {
        let count = accounts::Entity::find()
            .count(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(count as i64)
    }

    /// Resolve an account row by id or nickname (`label`), case-insensitive.
    pub async fn find_account(&self, identifier: &str) -> Result<Option<AccountRecord>> {
        let needle = identifier.to_ascii_lowercase();
        Ok(self.list_accounts().await?.into_iter().find(|a| {
            a.account_id.eq_ignore_ascii_case(identifier)
                || a.label
                    .as_ref()
                    .is_some_and(|l| l.eq_ignore_ascii_case(identifier))
                || a.account_id.to_ascii_lowercase() == needle
        }))
    }

    /// Toggle whether an account is included in automatic library scans.
    pub async fn set_scan_enabled(&self, account_id: &str, scan_enabled: bool) -> Result<()> {
        let model = accounts::Entity::find()
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
    pub async fn mark_connection_active(&self, account_id: &str) -> Result<()> {
        let model = accounts::Entity::find()
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
    pub async fn delete_account_secrets(&self, account_id: &str) -> Result<()> {
        crate::secrets::delete_secrets_for_account(&self.db, account_id).await
    }

    /// Mark bookstore credentials revoked without deleting the account or books.
    pub async fn revoke_credentials(&self, account_id: &str) -> Result<()> {
        let model = accounts::Entity::find()
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
        };
        let model = am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(map_portal_identity(model))
    }

    /// Create a first-party user.
    pub async fn create_user(
        &self,
        role: UserRole,
        display_name: Option<&str>,
        password_hash: Option<&str>,
    ) -> Result<UserRecord> {
        self.create_user_with_login(role, display_name, None, password_hash)
            .await
    }

    /// Create a first-party user with optional local login_name.
    pub async fn create_user_with_login(
        &self,
        role: UserRole,
        display_name: Option<&str>,
        login_name: Option<&str>,
        password_hash: Option<&str>,
    ) -> Result<UserRecord> {
        let now = now_str();
        let am = users::ActiveModel {
            id: NotSet,
            role: Set(role.as_str().to_string()),
            status: Set(UserStatus::Active.as_str().to_string()),
            display_name: Set(display_name.map(str::to_string)),
            login_name: Set(normalize_login_name(login_name)),
            password_hash: Set(password_hash.map(str::to_string)),
            security_version: Set(0),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        };
        let model = am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(map_user(model))
    }

    /// Look up a first-party user by id.
    pub async fn get_user(&self, id: i64) -> Result<Option<UserRecord>> {
        Ok(users::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_user))
    }

    /// Look up by local login_name (case-insensitive).
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

    /// Raw password hash for verification (never expose via API).
    pub async fn get_user_password_hash(&self, id: i64) -> Result<Option<String>> {
        Ok(users::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .and_then(|m| m.password_hash))
    }

    /// List all first-party users (admin tooling).
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

    /// Set user status (`active` / `disabled`).
    pub async fn set_user_status(&self, id: i64, status: UserStatus) -> Result<UserRecord> {
        let model = users::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound(format!("user {id}")))?;
        let mut am: users::ActiveModel = model.into();
        am.status = Set(status.as_str().to_string());
        am.updated_at = Set(now_str());
        let model = am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(map_user(model))
    }

    /// Set or clear Argon2id password hash; bumps `security_version`.
    pub async fn set_user_password_hash(
        &self,
        id: i64,
        password_hash: Option<&str>,
    ) -> Result<UserRecord> {
        let model = users::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound(format!("user {id}")))?;
        let next_sv = model.security_version.saturating_add(1);
        let mut am: users::ActiveModel = model.into();
        am.password_hash = Set(password_hash.map(str::to_string));
        am.security_version = Set(next_sv);
        am.updated_at = Set(now_str());
        let model = am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(map_user(model))
    }

    /// Set login_name (unique); does not bump security_version.
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
        let model = am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(map_user(model))
    }

    /// Delete all portal sessions for identities linked to `user_id`.
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

    /// Set user role (`administrator` / `member`).
    pub async fn set_user_role(&self, id: i64, role: UserRole) -> Result<UserRecord> {
        let model = users::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .ok_or_else(|| LibraryError::NotFound(format!("user {id}")))?;
        let mut am: users::ActiveModel = model.into();
        am.role = Set(role.as_str().to_string());
        am.updated_at = Set(now_str());
        let model = am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(map_user(model))
    }

    /// Count administrators (bootstrap guard).
    pub async fn count_administrators(&self) -> Result<u64> {
        users::Entity::find()
            .filter(users::Column::Role.eq(UserRole::Administrator.as_str()))
            .count(&self.db)
            .await
            .map_err(LibraryError::Orm)
    }

    /// Backfill `portal_identities.user_id` and remap prefs `portal:{id}` → `user:{id}`.
    ///
    /// Safe to call repeatedly after migrations. Does not link CLI-created store
    /// accounts that have no `account_links` (avoids widening access).
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
        };
        let model = am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(map_portal_identity(model))
    }

    /// Look up a portal identity.
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

    /// Insert a claim ticket (store only the hash).
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

    /// Create a portal session (hash only).
    pub async fn insert_portal_session(
        &self,
        token_hash: &str,
        identity_id: i64,
        expires_at: chrono::DateTime<Utc>,
    ) -> Result<()> {
        let am = portal_sessions::ActiveModel {
            id: NotSet,
            token_hash: Set(token_hash.to_string()),
            identity_id: Set(identity_id),
            expires_at: Set(expires_at.to_rfc3339()),
            created_at: Set(now_str()),
        };
        am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Delete a portal session by token hash (logout / revoke).
    pub async fn delete_portal_session(&self, token_hash: &str) -> Result<bool> {
        let result = portal_sessions::Entity::delete_many()
            .filter(portal_sessions::Column::TokenHash.eq(token_hash))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(result.rows_affected > 0)
    }

    /// List portal sessions for an identity (id, created_at, expires_at).
    pub async fn list_portal_sessions_for_identity(
        &self,
        identity_id: i64,
    ) -> Result<Vec<(i64, String, String)>> {
        let now = now_str();
        Ok(portal_sessions::Entity::find()
            .filter(portal_sessions::Column::IdentityId.eq(identity_id))
            .filter(portal_sessions::Column::ExpiresAt.gt(now))
            .order_by_desc(portal_sessions::Column::Id)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(|r| (r.id, r.created_at, r.expires_at))
            .collect())
    }

    /// Delete a portal session by id only if it belongs to `identity_id`.
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
    pub async fn count_account_links_for_account(&self, account_id: &str) -> Result<u64> {
        account_links::Entity::find()
            .filter(account_links::Column::AccountId.eq(account_id))
            .count(&self.db)
            .await
            .map_err(LibraryError::Orm)
    }

    /// Resolve a valid portal session to its identity.
    pub async fn get_portal_session_identity(
        &self,
        token_hash: &str,
    ) -> Result<Option<crate::models::PortalIdentity>> {
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
        self.get_portal_identity_by_id(session.identity_id).await
    }

    /// Create a durable operator session (hash only).
    pub async fn insert_operator_session(
        &self,
        token_hash: &str,
        expires_at: chrono::DateTime<Utc>,
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
        };
        am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Create an elevated operator session (Administrator reauth → short TTL).
    pub async fn insert_elevated_operator_session(
        &self,
        token_hash: &str,
        expires_at: chrono::DateTime<Utc>,
        elevated_from_user_id: i64,
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
        };
        am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Look up a valid operator session (touches `last_used_at`).
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
        let id = session.id;
        let expires_at = session.expires_at.clone();
        let created_at = session.created_at.clone();
        let elevated_from_user_id = session.elevated_from_user_id;
        let impersonating_user_id = session.impersonating_user_id;
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
        }))
    }

    /// Return whether a hashed operator session is still valid (and touch last_used).
    pub async fn operator_session_valid(&self, token_hash: &str) -> Result<bool> {
        Ok(self.get_operator_session(token_hash).await?.is_some())
    }

    /// Set or clear impersonation on an operator session.
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
        })
    }

    /// Delete an operator session by token hash (logout / revoke).
    pub async fn delete_operator_session(&self, token_hash: &str) -> Result<bool> {
        let result = operator_sessions::Entity::delete_many()
            .filter(operator_sessions::Column::TokenHash.eq(token_hash))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(result.rows_affected > 0)
    }

    /// Delete an operator session by row id.
    pub async fn delete_operator_session_by_id(&self, id: i64) -> Result<bool> {
        let result = operator_sessions::Entity::delete_by_id(id)
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(result.rows_affected > 0)
    }

    /// Delete expired operator sessions (bounded cleanup).
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
            })
            .collect())
    }

    /// First portal identity linked to a first-party user (for impersonation scoping).
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

    /// Append a security audit event.
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

    /// Upsert an OIDC client (public clients may omit secret hash).
    pub async fn upsert_oidc_client(
        &self,
        client_id: &str,
        client_secret_hash: Option<&str>,
        redirect_uris: &[String],
        name: Option<&str>,
    ) -> Result<()> {
        let uris = serde_json::to_string(redirect_uris)
            .map_err(|e| LibraryError::Other(anyhow::anyhow!("redirect_uris json: {e}")))?;
        if let Some(existing) = oidc_clients::Entity::find()
            .filter(oidc_clients::Column::ClientId.eq(client_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        {
            let mut am: oidc_clients::ActiveModel = existing.into();
            am.client_secret_hash = Set(client_secret_hash.map(str::to_string));
            am.redirect_uris_json = Set(uris);
            am.name = Set(name.map(str::to_string));
            am.update(&self.db).await.map_err(LibraryError::Orm)?;
            return Ok(());
        }
        let am = oidc_clients::ActiveModel {
            id: NotSet,
            client_id: Set(client_id.to_string()),
            client_secret_hash: Set(client_secret_hash.map(str::to_string)),
            redirect_uris_json: Set(uris),
            name: Set(name.map(str::to_string)),
            created_at: Set(now_str()),
        };
        am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    pub async fn get_oidc_client(
        &self,
        client_id: &str,
    ) -> Result<Option<(String, Option<String>, Vec<String>, Option<String>)>> {
        let Some(model) = oidc_clients::Entity::find()
            .filter(oidc_clients::Column::ClientId.eq(client_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        let uris: Vec<String> = serde_json::from_str(&model.redirect_uris_json).unwrap_or_default();
        Ok(Some((
            model.client_id,
            model.client_secret_hash,
            uris,
            model.name,
        )))
    }

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

    /// Atomically consume an auth code; returns (client_id, user_id, redirect_uri, challenge, method, scope).
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
    /// Account links are exclusive: an `account_id` may belong to only one
    /// identity (`idx_account_links_account_exclusive`).
    pub async fn link_account(
        &self,
        identity_id: i64,
        account_id: &str,
        source: &str,
    ) -> Result<crate::models::AccountLinkRecord> {
        let existing = account_links::Entity::find()
            .filter(account_links::Column::IdentityId.eq(identity_id))
            .filter(account_links::Column::AccountId.eq(account_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if let Some(model) = existing {
            return Ok(map_account_link(model));
        }
        if let Some(other) = account_links::Entity::find()
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
            account_id: Set(account_id.to_string()),
            source: Set(source.to_string()),
            created_at: Set(now_str()),
        };
        let model = am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(map_account_link(model))
    }

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
    pub async fn unlink_account(&self, identity_id: i64, account_id: &str) -> Result<()> {
        let res = account_links::Entity::delete_many()
            .filter(account_links::Column::IdentityId.eq(identity_id))
            .filter(account_links::Column::AccountId.eq(account_id))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if res.rows_affected == 0 {
            return Err(LibraryError::NotFound(account_id.into()));
        }
        Ok(())
    }

    /// Upsert a book from a library sync.
    ///
    /// Insert on first sight; on `(source, account_id, product_id)` conflict,
    /// merge fields with Audible-precedence rules (a Libro rescan without an
    /// ASIN must not wipe Audible enrichment). Expressed as load-then-merge over
    /// the `books` entity so the precedence logic is explicit and portable.
    pub async fn upsert_book(&self, book: &NewBook) -> Result<BookRecord> {
        // Only clone when HTML entities are present — hot scan paths stay allocation-light.
        let decoded;
        let book = if book.needs_html_entity_decode() {
            decoded = {
                let mut b = book.clone();
                b.decode_html_entities();
                b
            };
            &decoded
        } else {
            book
        };
        let now = now_str();
        let existing = books::Entity::find()
            .filter(books::Column::Source.eq(book.source.as_str()))
            .filter(books::Column::AccountId.eq(book.account_id.as_str()))
            .filter(books::Column::ProductId.eq(book.product_id.as_str()))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;

        let model = if let Some(existing) = existing {
            // `prefer_new`: incoming row is authoritative when it carries an
            // Audible ASIN, or the stored row never had one.
            let prefer_new = book.asin.is_some() || existing.asin.is_none();
            let keep =
                |new: Option<String>, old: Option<String>| if prefer_new { new } else { old };
            let coalesce_keep = |new: Option<String>, old: Option<String>| {
                if prefer_new {
                    new.or(old)
                } else {
                    old
                }
            };

            let mut am: books::ActiveModel = existing.clone().into();
            am.asin = Set(book.asin.clone().or(existing.asin.clone()));
            am.isbn = Set(book.isbn.clone().or(existing.isbn.clone()));
            am.marketplace = Set(book.marketplace.clone());
            am.title = Set(if prefer_new {
                book.title.clone()
            } else {
                existing.title.clone()
            });
            am.authors = Set(keep(book.authors.clone(), existing.authors.clone()));
            am.narrators = Set(keep(book.narrators.clone(), existing.narrators.clone()));
            am.series = Set(keep(book.series.clone(), existing.series.clone()));
            am.series_index = Set(keep(
                book.series_index.clone(),
                existing.series_index.clone(),
            ));
            am.series_asin = Set(coalesce_keep(
                book.series_asin.clone(),
                existing.series_asin.clone(),
            ));
            am.purchased_at = Set(book.purchased_at.map(|d| d.to_rfc3339()));
            am.publisher = Set(coalesce_keep(
                book.publisher.clone(),
                existing.publisher.clone(),
            ));
            am.length_minutes = Set(if prefer_new {
                book.length_minutes.or(existing.length_minutes)
            } else {
                existing.length_minutes
            });
            am.is_abridged = Set(i64::from(book.is_abridged));
            am.content_kind = Set(book.content_kind.clone());
            am.categories = Set(coalesce_keep(
                book.categories.clone(),
                existing.categories.clone(),
            ));
            am.subtitle = Set(coalesce_keep(
                book.subtitle.clone(),
                existing.subtitle.clone(),
            ));
            am.published_at = Set(if prefer_new {
                book.published_at
                    .map(|d| d.to_rfc3339())
                    .or(existing.published_at.clone())
            } else {
                existing.published_at.clone()
            });
            am.updated_at = Set(now);
            am.update(&self.db).await.map_err(LibraryError::Orm)?
        } else {
            let uuid = book
                .uuid
                .clone()
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            let am = books::ActiveModel {
                id: NotSet,
                uuid: Set(uuid),
                source: Set(book.source.clone()),
                account_id: Set(book.account_id.clone()),
                product_id: Set(book.product_id.clone()),
                asin: Set(book.asin.clone()),
                isbn: Set(book.isbn.clone()),
                marketplace: Set(book.marketplace.clone()),
                title: Set(book.title.clone()),
                authors: Set(book.authors.clone()),
                narrators: Set(book.narrators.clone()),
                series: Set(book.series.clone()),
                series_index: Set(book.series_index.clone()),
                series_asin: Set(book.series_asin.clone()),
                acquire_status: Set(AcquireStatus::NotAcquired.as_str().to_string()),
                storage_key: Set(None),
                error_message: Set(None),
                purchased_at: Set(book.purchased_at.map(|d| d.to_rfc3339())),
                tags: Set(None),
                rating_overall: Set(None),
                rating_performance: Set(None),
                rating_story: Set(None),
                is_finished: Set(0),
                pdf_status: Set(AcquireStatus::NotAcquired.as_str().to_string()),
                pdf_storage_key: Set(None),
                publisher: Set(book.publisher.clone()),
                length_minutes: Set(book.length_minutes),
                is_abridged: Set(i64::from(book.is_abridged)),
                content_kind: Set(book.content_kind.clone()),
                categories: Set(book.categories.clone()),
                subtitle: Set(book.subtitle.clone()),
                published_at: Set(book.published_at.map(|d| d.to_rfc3339())),
                description: Set(None),
                language: Set(None),
                cover_url: Set(None),
                subjects: Set(None),
                enrich_source: Set(None),
                enrich_confidence: Set(None),
                enrich_updated_at: Set(None),
                created_at: Set(now.clone()),
                updated_at: Set(now),
            };
            am.insert(&self.db).await.map_err(LibraryError::Orm)?
        };
        map_book(model)
    }

    /// Look up a book by its public `uuid`.
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
    /// Prefer exact uuid / product_id matches, then asin, then isbn; ties break
    /// by `source` ascending.
    pub async fn get_book(&self, title_id: &str, account_id: &str) -> Result<Option<BookRecord>> {
        let mut matches = books::Entity::find()
            .filter(books::Column::AccountId.eq(account_id))
            .filter(
                Condition::any()
                    .add(books::Column::Uuid.eq(title_id))
                    .add(books::Column::ProductId.eq(title_id))
                    .add(books::Column::Asin.eq(title_id))
                    .add(books::Column::Isbn.eq(title_id)),
            )
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        matches.sort_by(|a, b| {
            match_priority(a, title_id)
                .cmp(&match_priority(b, title_id))
                .then_with(|| a.source.cmp(&b.source))
        });
        matches.into_iter().next().map(map_book).transpose()
    }

    /// All ownership rows sharing an ISBN (cross-account / cross-store enrichment).
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
    pub async fn count_books(&self, account_id: Option<&str>) -> Result<i64> {
        let mut query = books::Entity::find();
        if let Some(account_id) = account_id {
            query = query.filter(books::Column::AccountId.eq(account_id));
        }
        let count = query.count(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(count as i64)
    }

    /// Resolve `title_id` to the stored public book model, or error if missing.
    async fn resolve_book(&self, title_id: &str, account_id: &str) -> Result<books::Model> {
        let mut matches = books::Entity::find()
            .filter(books::Column::AccountId.eq(account_id))
            .filter(
                Condition::any()
                    .add(books::Column::Uuid.eq(title_id))
                    .add(books::Column::ProductId.eq(title_id))
                    .add(books::Column::Asin.eq(title_id))
                    .add(books::Column::Isbn.eq(title_id)),
            )
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        matches.sort_by(|a, b| {
            match_priority(a, title_id)
                .cmp(&match_priority(b, title_id))
                .then_with(|| a.source.cmp(&b.source))
        });
        matches
            .into_iter()
            .next()
            .ok_or_else(|| LibraryError::NotFound(title_id.into()))
    }

    /// Update user-defined fields (tags, ratings, finished) without touching scan metadata.
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
        am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

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
        am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    pub async fn is_ignored(&self, title_id: &str, account_id: &str) -> Result<bool> {
        let (source, product_id) = self.ignore_key(title_id, account_id).await?;
        Ok(ignored_titles::Entity::find()
            .filter(ignored_titles::Column::Source.eq(source))
            .filter(ignored_titles::Column::AccountId.eq(account_id))
            .filter(ignored_titles::Column::ProductId.eq(product_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .is_some())
    }

    pub async fn set_ignored(
        &self,
        title_id: &str,
        account_id: &str,
        ignored: bool,
        reason: Option<&str>,
    ) -> Result<()> {
        let (source, product_id) = self.ignore_key(title_id, account_id).await?;
        if ignored {
            let existing = ignored_titles::Entity::find()
                .filter(ignored_titles::Column::Source.eq(source.as_str()))
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
                    source: Set(source),
                    account_id: Set(account_id.to_string()),
                    product_id: Set(product_id),
                    reason: Set(reason.map(str::to_string)),
                    created_at: Set(now_str()),
                };
                am.insert(&self.db).await.map_err(LibraryError::Orm)?;
            }
        } else {
            ignored_titles::Entity::delete_many()
                .filter(ignored_titles::Column::Source.eq(source))
                .filter(ignored_titles::Column::AccountId.eq(account_id))
                .filter(ignored_titles::Column::ProductId.eq(product_id))
                .exec(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
        }
        Ok(())
    }

    /// Resolve the `(source, product_id)` ignore key for a title, defaulting to
    /// Audible + the raw id when the book is unknown.
    async fn ignore_key(&self, title_id: &str, account_id: &str) -> Result<(String, String)> {
        Ok(match self.get_book(title_id, account_id).await? {
            Some(b) => (b.source, b.product_id),
            None => (String::from("audible"), title_id.to_string()),
        })
    }

    pub async fn set_acquire_status(
        &self,
        title_id: &str,
        account_id: &str,
        status: AcquireStatus,
        storage_key: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<()> {
        let model = self.resolve_book(title_id, account_id).await?;
        let mut am: books::ActiveModel = model.into();
        am.acquire_status = Set(status.as_str().to_string());
        am.storage_key = Set(storage_key.map(str::to_string));
        am.error_message = Set(error_message.map(str::to_string));
        am.updated_at = Set(now_str());
        am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Bulk-update acquire status (classic `set-status --force`).
    ///
    /// When `asins` is non-empty, matches against uuid, product_id, isbn, or asin.
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

    pub async fn get_saved_filter(&self, name: &str) -> Result<Option<SavedFilterRecord>> {
        Ok(saved_filters::Entity::find()
            .filter(saved_filters::Column::Name.eq(name))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_saved_filter))
    }

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

    pub async fn count_by_status(&self, status: AcquireStatus) -> Result<i64> {
        let count = books::Entity::find()
            .filter(books::Column::AcquireStatus.eq(status.as_str()))
            .count(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(count as i64)
    }

    /// Persist enrichment fields without touching scan / ownership columns.
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
        am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Upsert a canonical work row (COALESCE-preserve on conflict).
    pub async fn upsert_work(&self, work: &NewWork) -> Result<WorkRecord> {
        let now = now_str();
        let id = work
            .id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let existing = works::Entity::find_by_id(id.clone())
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let model = if let Some(existing) = existing {
            let coalesce = |new: Option<String>, old: Option<String>| new.or(old);
            let mut am: works::ActiveModel = existing.clone().into();
            am.canonical_asin = Set(coalesce(
                work.canonical_asin.clone(),
                existing.canonical_asin.clone(),
            ));
            am.canonical_isbn = Set(coalesce(
                work.canonical_isbn.clone(),
                existing.canonical_isbn.clone(),
            ));
            am.title = Set(work.title.clone());
            am.authors = Set(coalesce(work.authors.clone(), existing.authors.clone()));
            am.narrators = Set(coalesce(work.narrators.clone(), existing.narrators.clone()));
            am.description = Set(coalesce(
                work.description.clone(),
                existing.description.clone(),
            ));
            am.subjects = Set(coalesce(work.subjects.clone(), existing.subjects.clone()));
            am.categories = Set(coalesce(
                work.categories.clone(),
                existing.categories.clone(),
            ));
            am.language = Set(coalesce(work.language.clone(), existing.language.clone()));
            am.series = Set(coalesce(work.series.clone(), existing.series.clone()));
            am.series_index = Set(coalesce(
                work.series_index.clone(),
                existing.series_index.clone(),
            ));
            am.cover_url = Set(coalesce(work.cover_url.clone(), existing.cover_url.clone()));
            am.openlibrary_id = Set(coalesce(
                work.openlibrary_id.clone(),
                existing.openlibrary_id.clone(),
            ));
            am.updated_at = Set(now);
            am.update(&self.db).await.map_err(LibraryError::Orm)?
        } else {
            let am = works::ActiveModel {
                id: Set(id),
                canonical_asin: Set(work.canonical_asin.clone()),
                canonical_isbn: Set(work.canonical_isbn.clone()),
                title: Set(work.title.clone()),
                authors: Set(work.authors.clone()),
                narrators: Set(work.narrators.clone()),
                description: Set(work.description.clone()),
                subjects: Set(work.subjects.clone()),
                categories: Set(work.categories.clone()),
                language: Set(work.language.clone()),
                series: Set(work.series.clone()),
                series_index: Set(work.series_index.clone()),
                cover_url: Set(work.cover_url.clone()),
                openlibrary_id: Set(work.openlibrary_id.clone()),
                created_at: Set(now.clone()),
                updated_at: Set(now),
            };
            am.insert(&self.db).await.map_err(LibraryError::Orm)?
        };
        Ok(map_work(model))
    }

    pub async fn get_work(&self, id: &str) -> Result<Option<WorkRecord>> {
        Ok(works::Entity::find_by_id(id.to_string())
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_work))
    }

    pub async fn find_work_by_asin(&self, asin: &str) -> Result<Option<WorkRecord>> {
        Ok(works::Entity::find()
            .filter(works::Column::CanonicalAsin.eq(asin))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_work))
    }

    pub async fn find_work_by_isbn(&self, isbn: &str) -> Result<Option<WorkRecord>> {
        Ok(works::Entity::find()
            .filter(works::Column::CanonicalIsbn.eq(isbn))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_work))
    }

    pub async fn list_works(&self) -> Result<Vec<WorkRecord>> {
        Ok(works::Entity::find()
            .order_by_asc(works::Column::Title)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_work)
            .collect())
    }

    pub async fn link_book_to_work(&self, work_id: &str, book_uuid: &str) -> Result<()> {
        let existing = work_editions::Entity::find()
            .filter(work_editions::Column::BookUuid.eq(book_uuid))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if let Some(model) = existing {
            if model.work_id == work_id {
                return Ok(());
            }
            // The primary key includes work_id, so re-key by delete + insert.
            work_editions::Entity::delete_many()
                .filter(work_editions::Column::BookUuid.eq(book_uuid))
                .exec(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
        }
        let am = work_editions::ActiveModel {
            work_id: Set(work_id.to_string()),
            book_uuid: Set(book_uuid.to_string()),
            created_at: Set(now_str()),
        };
        am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    pub async fn work_id_for_book(&self, book_uuid: &str) -> Result<Option<String>> {
        Ok(work_editions::Entity::find()
            .filter(work_editions::Column::BookUuid.eq(book_uuid))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(|m| m.work_id))
    }

    pub async fn book_uuids_for_work(&self, work_id: &str) -> Result<Vec<String>> {
        Ok(work_editions::Entity::find()
            .filter(work_editions::Column::WorkId.eq(work_id))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(|m| m.book_uuid)
            .collect())
    }

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
            am.book_uuid = Set(coalesce(row.book_uuid.clone(), existing.book_uuid.clone()));
            am.work_id = Set(coalesce(row.work_id.clone(), existing.work_id.clone()));
            am.title = Set(coalesce(row.title.clone(), existing.title.clone()));
            am.authors = Set(coalesce(row.authors.clone(), existing.authors.clone()));
            am.asin = Set(coalesce(row.asin.clone(), existing.asin.clone()));
            am.isbn = Set(coalesce(row.isbn.clone(), existing.isbn.clone()));
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
                book_uuid: Set(row.book_uuid.clone()),
                work_id: Set(row.work_id.clone()),
                external_item_id: Set(row.external_item_id.clone()),
                title: Set(row.title.clone()),
                authors: Set(row.authors.clone()),
                asin: Set(row.asin.clone()),
                isbn: Set(row.isbn.clone()),
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

    pub async fn create_title_request(&self, req: &NewTitleRequest) -> Result<TitleRequestRecord> {
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
        let work_key = if req.work_key.trim().is_empty() {
            fallback_work_key(
                &req.title,
                req.authors.as_deref(),
                req.asin.as_deref(),
                req.isbn.as_deref(),
            )
        } else {
            req.work_key.trim().to_string()
        };

        // Idempotent wishlist: same wisher + exact work_key, or same bibliographic
        // identity under a different key (e.g. soft:… vs asin:… / isbn:…).
        if let Some(existing) = self.find_open_wishlist(req.identity_id, &work_key).await? {
            return self
                .backfill_title_request_cover(&existing, req.cover_url.as_deref())
                .await;
        }
        if let Some(existing) = self
            .find_open_wishlist_matching(
                req.identity_id,
                &work_key,
                &req.title,
                req.authors.as_deref(),
                req.asin.as_deref(),
                req.isbn.as_deref(),
            )
            .await?
        {
            return self
                .backfill_title_request_cover(&existing, req.cover_url.as_deref())
                .await;
        }

        let am = title_requests::ActiveModel {
            id: NotSet,
            uuid: Set(uuid),
            identity_id: Set(req.identity_id),
            title: Set(req.title.clone()),
            authors: Set(req.authors.clone()),
            asin: Set(req.asin.clone()),
            isbn: Set(req.isbn.clone()),
            notes: Set(req.notes.clone()),
            status: Set(req.status.as_str().to_string()),
            preferred_source: NotSet,
            work_id: Set(req.work_id.clone()),
            work_key: Set(work_key),
            resolved_book_uuid: Set(req.resolved_book_uuid.clone()),
            cover_url: Set(req.cover_url.clone()),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        };
        let model = am.insert(&self.db).await.map_err(LibraryError::Orm)?;
        self.get_title_request_by_id(model.id)
            .await?
            .ok_or_else(|| LibraryError::NotFound(model.uuid))
    }

    /// Upsert per-storefront snapshots for a wishlist row, then return the merged record.
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
            if model
                .asin
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_none()
            {
                if let Some(a) = row.asin.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    am.asin = Set(Some(a.to_string()));
                    dirty = true;
                }
            }
            if model
                .isbn
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_none()
            {
                if let Some(i) = row.isbn.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    am.isbn = Set(Some(i.to_string()));
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

    async fn get_title_request_by_id(&self, id: i64) -> Result<Option<TitleRequestRecord>> {
        let Some(model) = title_requests::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        let mut row = map_request(model);
        row.sources = self.list_title_request_sources(id).await?;
        apply_merged_sources(&mut row);
        Ok(Some(row))
    }

    async fn attach_sources_batch(
        &self,
        rows: Vec<TitleRequestRecord>,
    ) -> Result<Vec<TitleRequestRecord>> {
        if rows.is_empty() {
            return Ok(rows);
        }
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        let all = title_request_sources::Entity::find()
            .filter(title_request_sources::Column::TitleRequestId.is_in(ids))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let mut by_id: std::collections::HashMap<i64, Vec<TitleRequestSourceRecord>> =
            std::collections::HashMap::new();
        for m in all {
            by_id
                .entry(m.title_request_id)
                .or_default()
                .push(map_source(m));
        }
        Ok(rows
            .into_iter()
            .map(|mut row| {
                row.sources = by_id.remove(&row.id).unwrap_or_default();
                apply_merged_sources(&mut row);
                row
            })
            .collect())
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
        let mut row = map_request(model);
        row.sources = self.list_title_request_sources(row.id).await?;
        apply_merged_sources(&mut row);
        Ok(row)
    }

    /// Open wishlist row for this identity + work key, if any.
    pub async fn find_open_wishlist(
        &self,
        identity_id: Option<i64>,
        work_key: &str,
    ) -> Result<Option<TitleRequestRecord>> {
        let work_key = work_key.trim();
        if work_key.is_empty() {
            return Ok(None);
        }
        let mut query = title_requests::Entity::find()
            .filter(title_requests::Column::WorkKey.eq(work_key))
            .filter(title_requests::Column::Status.eq("open"))
            .order_by_desc(title_requests::Column::CreatedAt);
        query = match identity_id {
            Some(id) => query.filter(title_requests::Column::IdentityId.eq(id)),
            None => query.filter(title_requests::Column::IdentityId.is_null()),
        };
        let Some(model) = query.one(&self.db).await.map_err(LibraryError::Orm)? else {
            return Ok(None);
        };
        let mut row = map_request(model);
        row.sources = self.list_title_request_sources(row.id).await?;
        apply_merged_sources(&mut row);
        Ok(Some(row))
    }

    /// Open wishlist row that matches bibliographic identity even when `work_key` differs.
    pub async fn find_open_wishlist_matching(
        &self,
        identity_id: Option<i64>,
        work_key: &str,
        title: &str,
        authors: Option<&str>,
        asin: Option<&str>,
        isbn: Option<&str>,
    ) -> Result<Option<TitleRequestRecord>> {
        let needle = WishlistIdentity {
            work_key,
            title,
            authors,
            asin,
            isbn,
        };
        let open = self.list_wishlist(identity_id).await?;
        Ok(open.into_iter().find(|row| {
            needle.matches(WishlistIdentity {
                work_key: &row.work_key,
                title: &row.title,
                authors: row.authors.as_deref(),
                asin: row.asin.as_deref(),
                isbn: row.isbn.as_deref(),
            })
        }))
    }

    /// Personal open wishlist for a portal identity, or operator (`identity_id` null).
    pub async fn list_wishlist(&self, identity_id: Option<i64>) -> Result<Vec<TitleRequestRecord>> {
        let mut query = title_requests::Entity::find()
            .filter(title_requests::Column::Status.eq("open"))
            .order_by_desc(title_requests::Column::CreatedAt);
        query = match identity_id {
            Some(id) => query.filter(title_requests::Column::IdentityId.eq(id)),
            None => query.filter(title_requests::Column::IdentityId.is_null()),
        };
        let rows = query
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_request)
            .collect();
        self.attach_sources_batch(rows).await
    }

    /// Global request queue: open wishes grouped by `work_key`.
    ///
    /// Sorted by wish count as a simple default; Discover re-ranks with local
    /// taste plus a heavy per-wisher boost for the Wishlist sidebar.
    pub async fn list_global_request_queue(&self) -> Result<Vec<GlobalQueueEntry>> {
        let open = self.list_title_requests(Some(RequestStatus::Open)).await?;
        let mut by_key: std::collections::HashMap<String, GlobalQueueEntry> =
            std::collections::HashMap::new();
        for row in open {
            let key = if row.work_key.trim().is_empty() {
                fallback_work_key(
                    &row.title,
                    row.authors.as_deref(),
                    row.asin.as_deref(),
                    row.isbn.as_deref(),
                )
            } else {
                row.work_key.clone()
            };
            match by_key.entry(key.clone()) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(GlobalQueueEntry {
                        work_key: key,
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
                        sample_uuids: vec![row.uuid],
                        first_requested_at: row.created_at,
                        last_requested_at: row.created_at,
                    });
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    let entry = e.get_mut();
                    entry.wish_count += 1;
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
        let mut row = map_request(model);
        row.sources = self.list_title_request_sources(row.id).await?;
        apply_merged_sources(&mut row);
        Ok(Some(row))
    }

    pub async fn list_title_requests(
        &self,
        status: Option<RequestStatus>,
    ) -> Result<Vec<TitleRequestRecord>> {
        let mut query =
            title_requests::Entity::find().order_by_desc(title_requests::Column::CreatedAt);
        if let Some(status) = status {
            query = query.filter(title_requests::Column::Status.eq(status.as_str()));
        }
        let rows = query
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(map_request)
            .collect();
        self.attach_sources_batch(rows).await
    }

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
        am.update(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

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
    pub async fn get_user_preferences(&self, subject_key: &str) -> Result<Option<UserPreferences>> {
        Ok(user_preferences::Entity::find()
            .filter(user_preferences::Column::SubjectKey.eq(subject_key))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_user_preferences))
    }

    /// Preferences for `subject_key`, or in-memory defaults when no row exists.
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
    ) -> Result<UserPreferences> {
        let now = now_str();
        let shelves_json =
            serde_json::to_string(disabled_shelves).unwrap_or_else(|_| String::from("[]"));
        let excluded_json =
            serde_json::to_string(discover_excluded_sources).unwrap_or_else(|_| String::from("[]"));
        let sort = normalize_discover_sort(discover_sort);
        let sort_dir = normalize_discover_sort_dir(discover_sort_dir);
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
    /// Public id; generated on insert when `None`.
    pub uuid: Option<String>,
    pub product_id: String,
    pub source: String,
    pub account_id: String,
    pub asin: Option<String>,
    pub isbn: Option<String>,
    pub marketplace: String,
    pub title: String,
    pub authors: Option<String>,
    pub narrators: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<String>,
    pub series_asin: Option<String>,
    pub purchased_at: Option<chrono::DateTime<Utc>>,
    pub publisher: Option<String>,
    pub length_minutes: Option<i64>,
    pub is_abridged: bool,
    pub content_kind: String,
    pub categories: Option<String>,
    pub subtitle: Option<String>,
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
    /// `isbn = None`, `uuid = None` (auto), and `source = "audible"`.
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
    pub id: i64,
    pub name: String,
    pub query: String,
    pub created_at: chrono::DateTime<Utc>,
    pub updated_at: chrono::DateTime<Utc>,
}

/// Partial update for user-defined book fields.
#[derive(Debug, Clone, Default)]
pub struct UserBookFields {
    pub tags: Option<String>,
    pub rating_overall: Option<f32>,
    pub rating_performance: Option<f32>,
    pub rating_story: Option<f32>,
    pub is_finished: Option<bool>,
}

/// Durable catalog enrichment fields (blurbs, subjects, provenance).
#[derive(Debug, Clone, Default)]
pub struct CatalogEnrichmentFields {
    pub description: Option<String>,
    pub language: Option<String>,
    pub cover_url: Option<String>,
    pub subjects: Option<String>,
    pub categories: Option<String>,
    pub enrich_source: Option<String>,
    pub enrich_confidence: Option<f64>,
    pub enrich_updated_at: Option<chrono::DateTime<Utc>>,
}

/// Input for upserting a canonical work.
#[derive(Debug, Clone, Default)]
pub struct NewWork {
    pub id: Option<String>,
    pub canonical_asin: Option<String>,
    pub canonical_isbn: Option<String>,
    pub title: String,
    pub authors: Option<String>,
    pub narrators: Option<String>,
    pub description: Option<String>,
    pub subjects: Option<String>,
    pub categories: Option<String>,
    pub language: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<String>,
    pub cover_url: Option<String>,
    pub openlibrary_id: Option<String>,
}

/// Input for upserting listening progress.
#[derive(Debug, Clone)]
pub struct NewListeningProgress {
    pub identity_id: Option<i64>,
    pub provider: String,
    pub external_user_id: String,
    pub book_uuid: Option<String>,
    pub work_id: Option<String>,
    pub external_item_id: String,
    pub title: Option<String>,
    pub authors: Option<String>,
    pub asin: Option<String>,
    pub isbn: Option<String>,
    pub progress: Option<f64>,
    pub current_time_seconds: Option<f64>,
    pub duration_seconds: Option<f64>,
    pub is_finished: bool,
    pub last_listened_at: Option<chrono::DateTime<Utc>>,
}

/// Input for creating a title request / wishlist row.
#[derive(Debug, Clone)]
pub struct NewTitleRequest {
    pub uuid: Option<String>,
    pub identity_id: Option<i64>,
    pub title: String,
    pub authors: Option<String>,
    pub asin: Option<String>,
    pub isbn: Option<String>,
    pub notes: Option<String>,
    pub status: RequestStatus,
    /// Stable bibliographic key; empty triggers [`fallback_work_key`].
    pub work_key: String,
    pub work_id: Option<String>,
    pub resolved_book_uuid: Option<String>,
    pub cover_url: Option<String>,
}

impl NewTitleRequest {
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

/// Input for upserting a per-storefront wishlist snapshot.
#[derive(Debug, Clone, Default)]
pub struct NewTitleRequestSource {
    pub source: String,
    pub product_id: String,
    pub title: Option<String>,
    pub subtitle: Option<String>,
    pub authors: Option<String>,
    pub narrators: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<String>,
    pub asin: Option<String>,
    pub isbn: Option<String>,
    pub description: Option<String>,
    pub publisher: Option<String>,
    pub length_minutes: Option<i64>,
    pub published_at: Option<String>,
    pub categories: Option<String>,
    pub language: Option<String>,
    pub cover_url: Option<String>,
    pub url: Option<String>,
    pub price_cents: Option<i64>,
    pub currency: Option<String>,
    pub price_label: Option<String>,
    pub list_price_cents: Option<i64>,
    pub list_price_label: Option<String>,
    pub member_price_cents: Option<i64>,
    pub member_price_label: Option<String>,
}

impl NewTitleRequestSource {
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

/// Bibliographic slice used for wishlist dedupe.
#[derive(Debug, Clone, Copy)]
pub struct WishlistIdentity<'a> {
    pub work_key: &'a str,
    pub title: &'a str,
    pub authors: Option<&'a str>,
    pub asin: Option<&'a str>,
    pub isbn: Option<&'a str>,
}

impl WishlistIdentity<'_> {
    /// Whether two wishlist rows refer to the same work despite differing keys.
    ///
    /// Matches exact `work_key`, shared ASIN/ISBN, or the soft title+author key
    /// from [`fallback_work_key`]. Cross-key isbn↔asin pairs require a soft title
    /// agreement so unrelated hard keys never collapse.
    #[must_use]
    pub fn matches(self, other: WishlistIdentity<'_>) -> bool {
        let ka = self.work_key.trim();
        let kb = other.work_key.trim();
        if !ka.is_empty() && !kb.is_empty() && ka == kb {
            return true;
        }
        let soft_a = fallback_work_key(self.title, self.authors, None, None);
        let soft_b = fallback_work_key(other.title, other.authors, None, None);
        let soft_ok = soft_a != "soft:|" && soft_a == soft_b;

        if let (Some(aa), Some(ab)) = (
            self.asin.map(str::trim).filter(|s| !s.is_empty()),
            other.asin.map(str::trim).filter(|s| !s.is_empty()),
        ) {
            if aa.eq_ignore_ascii_case(ab) {
                return true;
            }
        }
        let isbn_a = fallback_work_key("", None, None, self.isbn);
        let isbn_b = fallback_work_key("", None, None, other.isbn);
        if isbn_a.starts_with("isbn:") && isbn_a == isbn_b {
            return true;
        }
        // Stored work_key may be asin:/isbn: while the other side only has fields.
        if !ka.is_empty() {
            if ka.starts_with("asin:") {
                if let Some(ab) = other.asin.map(str::trim).filter(|s| !s.is_empty()) {
                    if ka.eq_ignore_ascii_case(&format!("asin:{}", ab.to_ascii_uppercase())) {
                        return true;
                    }
                }
            }
            if ka.starts_with("isbn:") && ka == isbn_b {
                return true;
            }
            if ka.starts_with("soft:") && ka == soft_b {
                return true;
            }
        }
        if !kb.is_empty() {
            if kb.starts_with("asin:") {
                if let Some(aa) = self.asin.map(str::trim).filter(|s| !s.is_empty()) {
                    if kb.eq_ignore_ascii_case(&format!("asin:{}", aa.to_ascii_uppercase())) {
                        return true;
                    }
                }
            }
            if kb.starts_with("isbn:") && kb == isbn_a {
                return true;
            }
            if kb.starts_with("soft:") && kb == soft_a {
                return true;
            }
        }
        let cross = (ka.starts_with("isbn:") && kb.starts_with("asin:"))
            || (ka.starts_with("asin:") && kb.starts_with("isbn:"))
            || (isbn_a.starts_with("isbn:")
                && other.asin.map(str::trim).is_some_and(|s| !s.is_empty()))
            || (isbn_b.starts_with("isbn:")
                && self.asin.map(str::trim).is_some_and(|s| !s.is_empty()));
        if cross {
            return soft_ok;
        }
        soft_ok
    }
}

/// Whether two wishlist rows refer to the same work despite differing keys.
#[must_use]
pub fn wishlist_identities_match(a: WishlistIdentity<'_>, b: WishlistIdentity<'_>) -> bool {
    a.matches(b)
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

fn now_str() -> String {
    Utc::now().to_rfc3339()
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

fn parse_dt(value: &str) -> chrono::DateTime<Utc> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn parse_dt_opt(value: Option<&str>) -> Option<chrono::DateTime<Utc>> {
    value.map(parse_dt)
}

fn map_account(m: accounts::Model) -> AccountRecord {
    AccountRecord {
        id: m.id,
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

fn map_portal_identity(m: portal_identities::Model) -> crate::models::PortalIdentity {
    crate::models::PortalIdentity {
        id: m.id,
        provider: m.provider,
        external_user_id: m.external_user_id,
        label: m.label,
        user_id: m.user_id,
        created_at: parse_dt(&m.created_at),
    }
}

fn map_user(m: users::Model) -> UserRecord {
    UserRecord {
        id: m.id,
        role: UserRole::parse(&m.role).unwrap_or(UserRole::Member),
        status: UserStatus::parse(&m.status).unwrap_or(UserStatus::Active),
        display_name: m.display_name,
        login_name: m.login_name,
        has_password: m.password_hash.is_some(),
        security_version: m.security_version,
        created_at: parse_dt(&m.created_at),
        updated_at: parse_dt(&m.updated_at),
    }
}

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

fn map_account_link(m: account_links::Model) -> crate::models::AccountLinkRecord {
    crate::models::AccountLinkRecord {
        id: m.id,
        identity_id: m.identity_id,
        account_id: m.account_id,
        source: m.source,
        created_at: parse_dt(&m.created_at),
    }
}

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
        updated_at: parse_dt(&m.updated_at),
    }
}

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

fn normalize_discover_sort_dir(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "asc" | "ascending" => String::from("asc"),
        _ => String::from("desc"),
    }
}

fn map_saved_filter(m: saved_filters::Model) -> SavedFilterRecord {
    SavedFilterRecord {
        id: m.id,
        name: m.name,
        query: m.query,
        created_at: parse_dt(&m.created_at),
        updated_at: parse_dt(&m.updated_at),
    }
}

fn map_work(m: works::Model) -> WorkRecord {
    WorkRecord {
        id: m.id,
        canonical_asin: m.canonical_asin,
        canonical_isbn: m.canonical_isbn,
        title: m.title,
        authors: m.authors,
        narrators: m.narrators,
        description: m.description,
        subjects: m.subjects,
        categories: m.categories,
        language: m.language,
        series: m.series,
        series_index: m.series_index,
        cover_url: m.cover_url,
        openlibrary_id: m.openlibrary_id,
        created_at: parse_dt(&m.created_at),
        updated_at: parse_dt(&m.updated_at),
    }
}

fn map_listening(m: listening_progress::Model) -> ListeningProgressRecord {
    ListeningProgressRecord {
        id: m.id,
        identity_id: m.identity_id,
        provider: m.provider,
        external_user_id: m.external_user_id,
        book_uuid: m.book_uuid,
        work_id: m.work_id,
        external_item_id: m.external_item_id,
        title: m.title,
        authors: m.authors,
        asin: m.asin,
        isbn: m.isbn,
        progress: m.progress,
        current_time_seconds: m.current_time_seconds,
        duration_seconds: m.duration_seconds,
        is_finished: m.is_finished != 0,
        last_listened_at: parse_dt_opt(m.last_listened_at.as_deref()),
        updated_at: parse_dt(&m.updated_at),
    }
}

fn map_request(m: title_requests::Model) -> TitleRequestRecord {
    TitleRequestRecord {
        id: m.id,
        uuid: m.uuid,
        identity_id: m.identity_id,
        title: m.title,
        authors: m.authors,
        asin: m.asin,
        isbn: m.isbn,
        notes: m.notes,
        status: RequestStatus::parse(&m.status).unwrap_or_default(),
        work_key: m.work_key,
        work_id: m.work_id,
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

fn trim_opt(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

fn prefer_opt(current: Option<String>, incoming: Option<&str>) -> Option<String> {
    let next = trim_opt(incoming);
    match (current, next) {
        (None, n) => n,
        (c, None) => c,
        (Some(c), Some(n)) => Some(n).filter(|s| !s.is_empty()).or(Some(c)),
    }
}

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
        created_at: Set(created_at),
        updated_at: Set(now.to_string()),
    }
}

fn map_book(m: books::Model) -> Result<BookRecord> {
    Ok(BookRecord {
        id: m.id,
        uuid: m.uuid,
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
        subjects: m.subjects,
        enrich_source: m.enrich_source,
        enrich_confidence: m.enrich_confidence,
        enrich_updated_at: parse_dt_opt(m.enrich_updated_at.as_deref()),
        created_at: parse_dt(&m.created_at),
        updated_at: parse_dt(&m.updated_at),
    })
}

#[cfg(test)]
mod tests;
