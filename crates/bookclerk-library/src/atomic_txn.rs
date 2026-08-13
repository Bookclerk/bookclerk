//! Atomic library operations when interactive SeaORM transactions are unavailable.
//!
//! Cloudflare D1's HTTP API cannot keep `BEGIN` open across RPCs. The host
//! attaches a [`AtomicTxnBackend`] (the D1 guest's `dbAtomic` handler) so
//! claim redeem, last-owner guards, and related writes still commit as one
//! SQL transaction. SQLite and Postgres leave this unset and use SeaORM
//! `begin()` / `commit()`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::error::Result;
use crate::models::{PortalIdentity, UserRecord, UserRole, UserStatus};
use crate::SessionClientInfo;

/// Backend that runs [`crate::LibraryStore`] interactive transactions as a
/// single guest-side SQL batch (D1 HTTP `batch()`).
///
/// Implementations must preserve the same fail-closed semantics as the SeaORM
/// path: last-owner refusals mutate nothing; a failed claim redeem must not
/// consume the ticket or write a password/session.
#[async_trait]
pub trait AtomicTxnBackend: Send + Sync {
    /// Delete a first-party user and personal data (last-owner guarded).
    async fn delete_user(&self, id: i64) -> Result<()>;

    /// Set user status (`active` / `disabled`; last-owner guarded).
    async fn set_user_status(&self, id: i64, status: UserStatus) -> Result<UserRecord>;

    /// Set or clear the Argon2id password hash and bump `security_version`.
    async fn set_user_password_hash(
        &self,
        id: i64,
        password_hash: Option<&str>,
    ) -> Result<UserRecord>;

    /// Set user role (last-owner guarded on demotion).
    async fn set_user_role(&self, id: i64, role: UserRole) -> Result<UserRecord>;

    /// Consume a claim ticket, optionally set a first password, mint a session.
    async fn redeem_claim_ticket_to_session(
        &self,
        token_hash: &str,
        session_hash: &str,
        expires_at: DateTime<Utc>,
        client: Option<&SessionClientInfo>,
        new_password_hash: Option<&str>,
    ) -> Result<PortalIdentity>;
}
