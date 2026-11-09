//! Named atomic operations over any runner of compiled plans.
//!
//! [`AtomicTxnBackend`] has one method per named security operation. Every
//! backend builds the operation's [`DbAtomicParams`], compiles them with
//! [`compile_named_request`], runs the plan as one atomic batch and reads
//! the status and payload back the same way. That per-operation code lives
//! here once, over a [`NamedAtomicRunner`] that only knows how to run one
//! compiled plan: the in-process connection of a local tool, or the
//! profile-enforcing executor of the test kit.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::atomic_ops::{atomic_status, DbAtomicParams, DbAtomicResult};
use crate::atomic_txn::AtomicTxnBackend;
use crate::db_atomic::{
    app_err, confirm_totp_params, db_atomic_operation_id, decode_payload, identity_from_atomic,
    unit_from_atomic, user_from_atomic, AtomicOidcRpState, AtomicWebauthnChallenge,
};
use crate::error::{LibraryError, Result};
use crate::models::{
    AcquireStatus, EnqueueJobSpec, EnqueueOutcome, EventDeliveryRecord, EventSubscriber, JobRecord,
    JobResourceClass, PortalIdentity, PublishDomainEventOutcome, PublishDomainEventSpec,
    UserRecord, UserRole, UserStatus,
};
use crate::secrets::EncryptedSecretRecord;
use crate::sql_plan::{compile_claim_event_delivery, compile_named_request, CompiledAtomic};
use crate::SessionClientInfo;

/// Runs one compiled named plan as a single atomic batch.
///
/// A runner is the transport under a [`NamedAtomicBackend`]: it receives the
/// plan the backend compiled and answers with the interpreted result. It
/// must apply the whole plan or none of it, and must return application
/// outcomes (`notFound`, `lastOwner`, `empty`, …) as the result's status,
/// not as errors.
#[async_trait]
pub trait NamedAtomicRunner: Send + Sync {
    /// Runs `compiled` under `operation_id`, the idempotency key a retry
    /// after a lost reply resumes.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::LimitExceeded`] when the plan or its result
    /// exceeds a negotiated cap, [`LibraryError::Unavailable`] when the reply
    /// was lost and the batch may have been applied, and another
    /// [`LibraryError`] when the engine refused the batch.
    async fn run_compiled(
        &self,
        compiled: CompiledAtomic,
        operation_id: String,
    ) -> Result<DbAtomicResult>;
}

/// An [`AtomicTxnBackend`] that compiles every named operation and hands the
/// plan to a [`NamedAtomicRunner`].
#[derive(Debug)]
pub struct NamedAtomicBackend<R> {
    /// Runs the compiled plans.
    runner: R,
}

impl<R> NamedAtomicBackend<R> {
    /// Wraps `runner`.
    #[must_use]
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    /// The runner under this backend.
    #[must_use]
    pub fn runner(&self) -> &R {
        &self.runner
    }
}

impl<R: NamedAtomicRunner> NamedAtomicBackend<R> {
    /// Compiles and runs `params` under the operation's own idempotency key
    /// (see [`db_atomic_operation_id`]).
    async fn call(&self, params: DbAtomicParams) -> Result<DbAtomicResult> {
        let operation_id = db_atomic_operation_id(&params);
        self.call_with_id(operation_id, params).await
    }

    /// Compiles and runs `params` under a caller-chosen idempotency key, for
    /// claims whose retry must resume the same receipt.
    async fn call_with_id(
        &self,
        operation_id: String,
        params: DbAtomicParams,
    ) -> Result<DbAtomicResult> {
        let now = Utc::now().to_rfc3339();
        let compiled =
            compile_named_request(&operation_id, &params, &now).map_err(LibraryError::Orm)?;
        self.runner.run_compiled(compiled, operation_id).await
    }
}

/// Reads the `id` string out of an `ok` or `duplicate` payload.
fn payload_id(result: &DbAtomicResult, missing: &str) -> Result<String> {
    result
        .payload
        .as_ref()
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| LibraryError::Other(anyhow::anyhow!("{missing}")))
}

/// Serializes a value the plan carries as a JSON string column.
fn json_string<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|err| LibraryError::Other(anyhow::anyhow!(err.to_string())))
}

#[async_trait]
impl<R: NamedAtomicRunner> AtomicTxnBackend for NamedAtomicBackend<R> {
    async fn delete_user(&self, id: i64) -> Result<()> {
        unit_from_atomic(
            self.call(DbAtomicParams::DeleteUser { user_id: id })
                .await?,
            format!("user {id}"),
        )
    }

    async fn set_user_status(&self, id: i64, status: UserStatus) -> Result<UserRecord> {
        user_from_atomic(
            self.call(DbAtomicParams::SetUserStatus {
                user_id: id,
                status: status.as_str().to_string(),
            })
            .await?,
            format!("user {id}"),
        )
    }

    async fn set_user_password_hash(
        &self,
        id: i64,
        password_hash: Option<&str>,
    ) -> Result<UserRecord> {
        user_from_atomic(
            self.call(DbAtomicParams::SetUserPasswordHash {
                user_id: id,
                password_hash: password_hash.map(str::to_string),
            })
            .await?,
            format!("user {id}"),
        )
    }

    async fn set_user_role(&self, id: i64, role: UserRole) -> Result<UserRecord> {
        user_from_atomic(
            self.call(DbAtomicParams::SetUserRole {
                user_id: id,
                role: role.as_str().to_string(),
            })
            .await?,
            format!("user {id}"),
        )
    }

    async fn redeem_claim_ticket_to_session(
        &self,
        token_hash: &str,
        session_hash: &str,
        expires_at: DateTime<Utc>,
        client: Option<&SessionClientInfo>,
        new_password_hash: Option<&str>,
        password_fingerprint: Option<&str>,
    ) -> Result<PortalIdentity> {
        identity_from_atomic(
            self.call(DbAtomicParams::RedeemClaimTicket {
                token_hash: token_hash.to_string(),
                session_hash: session_hash.to_string(),
                expires_at: expires_at.to_rfc3339(),
                user_agent: client.and_then(|c| c.user_agent.clone()),
                device_type: client.map(|c| c.device_type.clone()),
                client_label: client.map(|c| c.client_label.clone()),
                new_password_hash: new_password_hash.map(str::to_string),
                password_fingerprint: password_fingerprint.map(str::to_string),
            })
            .await?,
        )
    }

    async fn take_oidc_rp_state(
        &self,
        state_hash: &str,
    ) -> Result<Option<(String, String, String, String, Option<i64>)>> {
        let result = self
            .call(DbAtomicParams::TakeOidcRpState {
                state_hash: state_hash.to_string(),
            })
            .await?;
        if result.status == atomic_status::EMPTY {
            return Ok(None);
        }
        if let Some(err) = app_err(&result.status, "oidc rp state".into()) {
            return Err(err);
        }
        let row: AtomicOidcRpState = decode_payload(result.payload, "oidc rp state")?;
        Ok(Some((
            row.provider_id,
            row.pkce_verifier,
            row.nonce,
            row.purpose,
            row.user_id,
        )))
    }

    async fn take_webauthn_challenge(
        &self,
        challenge_id: &str,
        kind: &str,
    ) -> Result<Option<(Option<i64>, String)>> {
        let result = self
            .call(DbAtomicParams::TakeWebauthnChallenge {
                challenge_id: challenge_id.to_string(),
                kind: kind.to_string(),
            })
            .await?;
        if result.status == atomic_status::EMPTY {
            return Ok(None);
        }
        if let Some(err) = app_err(&result.status, "webauthn challenge".into()) {
            return Err(err);
        }
        let row: AtomicWebauthnChallenge = decode_payload(result.payload, "webauthn challenge")?;
        Ok(Some((row.user_id, row.state_json)))
    }

    async fn enqueue_job(&self, spec: EnqueueJobSpec) -> Result<EnqueueOutcome> {
        let result = self
            .call(DbAtomicParams::EnqueueJob {
                kind: spec.kind.as_str().to_string(),
                payload_json: json_string(&spec.payload)?,
                priority: spec.priority,
                max_attempts: spec.max_attempts,
                max_pending: spec.max_pending,
                run_after: spec.run_after.map(|t| t.to_rfc3339()),
            })
            .await?;
        match result.status.as_str() {
            s if s == atomic_status::OK => Ok(EnqueueOutcome::Created {
                id: payload_id(&result, "enqueueJob ok without id")?,
            }),
            s if s == atomic_status::DUPLICATE => Ok(EnqueueOutcome::Duplicate {
                existing_id: payload_id(&result, "enqueueJob duplicate without id")?,
            }),
            s if s == atomic_status::QUEUE_FULL => Ok(EnqueueOutcome::QueueFull),
            other => Err(LibraryError::Other(anyhow::anyhow!(
                "enqueueJob failed: {other}"
            ))),
        }
    }

    async fn claim_next_job(
        &self,
        resource_class: JobResourceClass,
        owner: &str,
        lease_secs: u64,
        operation_id: &str,
    ) -> Result<Option<JobRecord>> {
        let result = self
            .call_with_id(
                operation_id.to_string(),
                DbAtomicParams::ClaimNextJob {
                    resource_class: resource_class.as_str().to_string(),
                    owner: owner.to_string(),
                    lease_secs: i64::try_from(lease_secs).unwrap_or(60),
                },
            )
            .await?;
        if result.status == atomic_status::EMPTY {
            return Ok(None);
        }
        if let Some(err) = app_err(&result.status, "job".into()) {
            return Err(err);
        }
        decode_payload(result.payload, "job").map(Some)
    }

    async fn reserve_job_temp_path(
        &self,
        job_id: &str,
        path: &str,
        reserved_bytes: u64,
        quota_bytes: u64,
    ) -> Result<()> {
        let result = self
            .call(DbAtomicParams::ReserveJobTemp {
                job_id: job_id.to_string(),
                path: path.to_string(),
                reserved_bytes: i64::try_from(reserved_bytes).unwrap_or(i64::MAX),
                quota_bytes: i64::try_from(quota_bytes).unwrap_or(i64::MAX),
            })
            .await?;
        // The plan answers `notFound` when the reservation would push the
        // job over its scratch quota.
        match app_err(&result.status, String::new()) {
            None => Ok(()),
            Some(LibraryError::NotFound(_)) => Err(LibraryError::Other(anyhow::anyhow!(
                "acquire scratch quota exceeded"
            ))),
            Some(err) => Err(err),
        }
    }

    async fn confirm_totp_enrollment(
        &self,
        user_id: i64,
        record: &EncryptedSecretRecord,
    ) -> Result<()> {
        unit_from_atomic(
            self.call(confirm_totp_params(user_id, record)).await?,
            "user".into(),
        )
    }

    async fn disable_user_totp(&self, user_id: i64) -> Result<()> {
        unit_from_atomic(
            self.call(DbAtomicParams::DisableUserTotp { user_id })
                .await?,
            "user".into(),
        )
    }

    async fn publish_domain_event(
        &self,
        spec: PublishDomainEventSpec,
    ) -> Result<PublishDomainEventOutcome> {
        let spec = crate::prepare_publish_domain_event(spec)?;
        let deliver_after = crate::deliver_after_for(&spec, &Utc::now().to_rfc3339());
        let result = self
            .call(DbAtomicParams::PublishDomainEvent {
                id: spec.id,
                event_type: spec.event_type,
                schema_version: spec.schema_version,
                account_id: spec.account_id,
                source: spec.source,
                correlation_id: spec.correlation_id,
                causation_id: spec.causation_id,
                dedup_key: spec.dedup_key,
                payload: spec.payload,
                ordering_key: spec.ordering_key,
                queue_name: spec.queue_name,
                content_type: spec.content_type,
                deliver_after,
            })
            .await?;
        match result.status.as_str() {
            s if s == atomic_status::OK => Ok(PublishDomainEventOutcome::Created {
                id: payload_id(&result, "publishDomainEvent ok without id")?,
            }),
            s if s == atomic_status::DUPLICATE => Ok(PublishDomainEventOutcome::Duplicate {
                existing_id: payload_id(&result, "publishDomainEvent duplicate without id")?,
            }),
            other => Err(LibraryError::Other(anyhow::anyhow!(
                "publishDomainEvent failed: {other}"
            ))),
        }
    }

    async fn set_acquire_status(
        &self,
        book_uuid: &str,
        status: AcquireStatus,
        storage_key: Option<&str>,
        error_message: Option<&str>,
        event: Option<PublishDomainEventSpec>,
    ) -> Result<()> {
        let event = match event {
            Some(spec) => crate::prepare_publish_domain_event(spec)?,
            None => PublishDomainEventSpec::default(),
        };
        let result = self
            .call(DbAtomicParams::SetAcquireStatus {
                book_uuid: book_uuid.to_string(),
                status: status.as_str().to_string(),
                storage_key: storage_key.map(str::to_string),
                error_message: error_message.map(str::to_string),
                event_id: event.id,
                event_type: event.event_type,
                schema_version: event.schema_version,
                event_account_id: event.account_id,
                source: event.source,
                correlation_id: event.correlation_id,
                causation_id: event.causation_id,
                dedup_key: event.dedup_key,
                payload: event.payload,
                ordering_key: event.ordering_key,
            })
            .await?;
        unit_from_atomic(result, format!("book {book_uuid}"))
    }

    async fn dispatch_event_deliveries(
        &self,
        event_id: &str,
        subscribers: &[EventSubscriber],
        operation_id: &str,
        mark_dispatched: bool,
    ) -> Result<u32> {
        let result = self
            .call_with_id(
                operation_id.to_string(),
                DbAtomicParams::DispatchEventDeliveries {
                    event_id: event_id.to_string(),
                    subscribers_json: json_string(&subscribers)?,
                    mark_dispatched,
                },
            )
            .await?;
        if let Some(err) = app_err(&result.status, format!("event {event_id}")) {
            return Err(err);
        }
        let created = result
            .payload
            .as_ref()
            .and_then(|v| v.get("created"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        Ok(u32::try_from(created).unwrap_or(u32::MAX))
    }

    async fn link_listing(&self, params: crate::library_link::ListingLinkParams) -> Result<String> {
        let result = self
            .call(DbAtomicParams::LinkListing(Box::new(params)))
            .await?;
        Ok(result.status)
    }

    async fn write_credits(&self, params: crate::library_link::CreditBatchParams) -> Result<()> {
        let result = self
            .call(DbAtomicParams::WriteCredits(Box::new(params)))
            .await?;
        if result.status == atomic_status::OK {
            Ok(())
        } else {
            Err(LibraryError::Other(anyhow::anyhow!(
                "credit batch answered `{}`",
                result.status
            )))
        }
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
        let now = Utc::now().to_rfc3339();
        let compiled = compile_claim_event_delivery(
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
        let result = self
            .runner
            .run_compiled(compiled, operation_id.to_string())
            .await?;
        if result.status == atomic_status::EMPTY {
            return Ok(None);
        }
        if let Some(err) = app_err(&result.status, "event delivery".into()) {
            return Err(err);
        }
        decode_payload(result.payload, "event delivery").map(Some)
    }
}
