//! In-process SQLite atomic backend for tests (no plugin RPC).

use async_trait::async_trait;
use sea_orm::DatabaseConnection;

use crate::atomic_ops::atomic_status;
use crate::atomic_txn::AtomicTxnBackend;
use crate::error::{LibraryError, Result};
use crate::models::{
    EnqueueJobSpec, EnqueueOutcome, EventDeliveryRecord, EventSubscriber, JobRecord,
    JobResourceClass, PortalIdentity, PublishDomainEventOutcome, PublishDomainEventSpec,
    UserRecord, UserRole, UserStatus,
};
use crate::{execute_db_atomic, SessionClientInfo};

/// Compiles named ops and runs them on a local SeaORM SQLite connection.
pub struct InProcessSqliteAtomic {
    /// Open library connection (already migrated).
    pub db: DatabaseConnection,
}

/// Fail closed for operations this in-process helper does not compile.
fn unsupported<T>(op: &str) -> Result<T> {
    Err(LibraryError::Other(anyhow::anyhow!(
        "InProcessSqliteAtomic does not implement {op}"
    )))
}

#[async_trait]
impl AtomicTxnBackend for InProcessSqliteAtomic {
    async fn delete_user(&self, _id: i64) -> Result<()> {
        unsupported("delete_user")
    }

    async fn set_user_status(&self, _id: i64, _status: UserStatus) -> Result<UserRecord> {
        unsupported("set_user_status")
    }

    async fn set_user_password_hash(
        &self,
        _id: i64,
        _password_hash: Option<&str>,
    ) -> Result<UserRecord> {
        unsupported("set_user_password_hash")
    }

    async fn set_user_role(&self, _id: i64, _role: UserRole) -> Result<UserRecord> {
        unsupported("set_user_role")
    }

    async fn redeem_claim_ticket_to_session(
        &self,
        _token_hash: &str,
        _session_hash: &str,
        _expires_at: chrono::DateTime<chrono::Utc>,
        _client: Option<&SessionClientInfo>,
        _new_password_hash: Option<&str>,
        _password_fingerprint: Option<&str>,
    ) -> Result<PortalIdentity> {
        unsupported("redeem_claim_ticket_to_session")
    }

    async fn take_oidc_rp_state(
        &self,
        _state_hash: &str,
    ) -> Result<Option<(String, String, String, String, Option<i64>)>> {
        unsupported("take_oidc_rp_state")
    }

    async fn take_webauthn_challenge(
        &self,
        _challenge_id: &str,
        _kind: &str,
    ) -> Result<Option<(Option<i64>, String)>> {
        unsupported("take_webauthn_challenge")
    }

    async fn enqueue_job(&self, _spec: EnqueueJobSpec) -> Result<EnqueueOutcome> {
        unsupported("enqueue_job")
    }

    async fn claim_next_job(
        &self,
        _resource_class: JobResourceClass,
        _owner: &str,
        _lease_secs: u64,
        _operation_id: &str,
    ) -> Result<Option<JobRecord>> {
        unsupported("claim_next_job")
    }

    async fn reserve_job_temp_path(
        &self,
        _job_id: &str,
        _path: &str,
        _reserved_bytes: u64,
        _quota_bytes: u64,
    ) -> Result<()> {
        unsupported("reserve_job_temp_path")
    }

    async fn confirm_totp_enrollment(
        &self,
        _user_id: i64,
        _record: &crate::secrets::EncryptedSecretRecord,
    ) -> Result<()> {
        unsupported("confirm_totp_enrollment")
    }

    async fn disable_user_totp(&self, _user_id: i64) -> Result<()> {
        unsupported("disable_user_totp")
    }

    async fn publish_domain_event(
        &self,
        _spec: PublishDomainEventSpec,
    ) -> Result<PublishDomainEventOutcome> {
        unsupported("publish_domain_event")
    }

    async fn set_acquire_status(
        &self,
        _book_uuid: &str,
        _status: crate::AcquireStatus,
        _storage_key: Option<&str>,
        _error_message: Option<&str>,
        _event: Option<PublishDomainEventSpec>,
    ) -> Result<()> {
        unsupported("set_acquire_status")
    }

    async fn dispatch_event_deliveries(
        &self,
        event_id: &str,
        subscribers: &[EventSubscriber],
        operation_id: &str,
        mark_dispatched: bool,
    ) -> Result<u32> {
        let subscribers_json = serde_json::to_string(subscribers)
            .map_err(|err| LibraryError::Other(anyhow::anyhow!(err.to_string())))?;
        let now = chrono::Utc::now().to_rfc3339();
        let compiled = crate::compile_named_request(
            operation_id,
            &crate::DbAtomicParams::DispatchEventDeliveries {
                event_id: event_id.to_string(),
                subscribers_json,
                mark_dispatched,
            },
            &now,
        )
        .map_err(LibraryError::Orm)?;
        crate::validate_plan(
            &compiled,
            &bookclerk_plugin_abi::DbCapabilities::advertised_sqlite(),
        )?;
        let result = execute_db_atomic(&self.db, compiled).await?;
        if result.status == atomic_status::NOT_FOUND {
            return Err(LibraryError::NotFound(format!("event {event_id}")));
        }
        if result.status != atomic_status::OK {
            return Err(LibraryError::Other(anyhow::anyhow!(
                "database atomic dispatch failed: {}",
                result.status
            )));
        }
        Ok(result
            .payload
            .as_ref()
            .and_then(|v| v.get("created"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32)
    }

    #[allow(clippy::too_many_arguments)]
    async fn claim_event_delivery(
        &self,
        delivery_id: &str,
        owner: &str,
        lease_secs: u64,
        operation_id: &str,
        plugin_id: &str,
        resource_class: &str,
        max_in_flight: u32,
    ) -> Result<Option<EventDeliveryRecord>> {
        let now = chrono::Utc::now().to_rfc3339();
        let compiled = crate::compile_claim_event_delivery(
            operation_id,
            delivery_id,
            owner,
            i64::try_from(lease_secs).unwrap_or(60),
            plugin_id,
            resource_class,
            i64::from(max_in_flight),
            &now,
        )
        .map_err(LibraryError::Orm)?;
        let result = execute_db_atomic(&self.db, compiled).await?;
        if result.status == atomic_status::EMPTY {
            return Ok(None);
        }
        if result.status != atomic_status::OK {
            return Err(LibraryError::Other(anyhow::anyhow!(
                "database atomic claim failed: {}",
                result.status
            )));
        }
        let payload = result.payload.ok_or_else(|| {
            LibraryError::Other(anyhow::anyhow!("database atomic claim missing payload"))
        })?;
        serde_json::from_value(payload).map(Some).map_err(|err| {
            LibraryError::Other(anyhow::anyhow!("database atomic claim payload: {err}"))
        })
    }
}
