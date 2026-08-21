//! Atomic library operations for named security commands.
//!
//! Database guests implement [`crate::LibraryStore`] interactive transactions as
//! a single `dbAtomic` RPC. D1 compiles the command to one HTTP `batch()`;
//! SQLite and PostgreSQL run it in a native local transaction. Both write a
//! durable receipt keyed by `operationId` so a committed result can be replayed
//! after a lost response. Generic `dbBegin` / `dbCommit` remain for unrelated
//! work.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::error::Result;
use crate::models::{
    EnqueueJobSpec, EnqueueOutcome, EventDeliveryRecord, EventSubscriber, JobRecord,
    JobResourceClass, PortalIdentity, PublishDomainEventOutcome, PublishDomainEventSpec,
    UserRecord, UserRole, UserStatus,
};
use crate::secrets::EncryptedSecretRecord;
use crate::SessionClientInfo;

/// Backend that runs [`crate::LibraryStore`] named security operations as a
/// single guest `dbAtomic` command.
///
/// Implementations must preserve the same fail-closed semantics as the SeaORM
/// path: last-owner refusals mutate nothing; a failed claim redeem must not
/// consume the ticket or write a password/session; consume-once OIDC/WebAuthn
/// rows must not be observable by a concurrent caller; TOTP enroll/disable must
/// keep `encrypted_secrets` and `users.totp_enabled` in the same transaction.
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
        password_fingerprint: Option<&str>,
    ) -> Result<PortalIdentity>;

    /// Consume a one-time OIDC RP state. `Ok(None)` if missing or expired.
    ///
    /// Tuple is `(provider_id, pkce_verifier, nonce, purpose, user_id)`.
    async fn take_oidc_rp_state(
        &self,
        state_hash: &str,
    ) -> Result<Option<(String, String, String, String, Option<i64>)>>;

    /// Consume a one-time WebAuthn challenge. `Ok(None)` if missing or expired.
    ///
    /// Tuple is `(user_id, state_json)`.
    async fn take_webauthn_challenge(
        &self,
        challenge_id: &str,
        kind: &str,
    ) -> Result<Option<(Option<i64>, String)>>;

    /// Admit a durable job in one `dbAtomic` transaction.
    async fn enqueue_job(&self, spec: EnqueueJobSpec) -> Result<EnqueueOutcome>;

    /// Claim the next ready job; `operation_id` makes a lost response replay-safe.
    async fn claim_next_job(
        &self,
        resource_class: JobResourceClass,
        owner: &str,
        lease_secs: u64,
        operation_id: &str,
    ) -> Result<Option<JobRecord>>;

    /// Reserve scratch-quota bytes for `path` on `job_id`.
    async fn reserve_job_temp_path(
        &self,
        job_id: &str,
        path: &str,
        reserved_bytes: u64,
        quota_bytes: u64,
    ) -> Result<()>;

    /// Promote a sealed TOTP secret to `primary` and set `totp_enabled`.
    async fn confirm_totp_enrollment(
        &self,
        user_id: i64,
        record: &EncryptedSecretRecord,
    ) -> Result<()>;

    /// Delete TOTP secrets and clear `totp_enabled`.
    async fn disable_user_totp(&self, user_id: i64) -> Result<()>;

    /// Persist a domain event in the outbox.
    async fn publish_domain_event(
        &self,
        spec: PublishDomainEventSpec,
    ) -> Result<PublishDomainEventOutcome>;

    /// Update acquire status and, when acquired, publish `book_acquired` in the same transaction.
    async fn set_acquire_status(
        &self,
        book_uuid: &str,
        status: crate::models::AcquireStatus,
        storage_key: Option<&str>,
        error_message: Option<&str>,
        event: Option<PublishDomainEventSpec>,
    ) -> Result<()>;

    /// Create deliveries for `subscribers` and mark the event dispatched.
    async fn dispatch_event_deliveries(
        &self,
        event_id: &str,
        subscribers: &[EventSubscriber],
        operation_id: &str,
    ) -> Result<u32>;

    /// Claim the next ready event delivery; `operation_id` makes a lost response replay-safe.
    async fn claim_next_event_delivery(
        &self,
        owner: &str,
        lease_secs: u64,
        operation_id: &str,
        plugin_ids: &[String],
        max_in_flight: u32,
    ) -> Result<Option<EventDeliveryRecord>>;
}
