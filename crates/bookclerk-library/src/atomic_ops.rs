//! Host-owned named atomic operations and interpreted results.
//!
//! Database guests execute generic [`crate::sql_plan::DbAtomicPlan`]
//! statements and return [`crate::sql_plan::DbPlanExecResult`]. The host
//! compiles [`DbAtomicParams`] and interprets [`DbAtomicResult`].

use crate::sql_plan::DbAtomicTiming;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// Application status strings produced by host plan interpretation.
pub mod atomic_status {
    /// Operation committed with the expected payload (or no payload).
    pub const OK: &str = "ok";
    /// Consume-once lookup found nothing (or the row was expired).
    pub const EMPTY: &str = "empty";
    /// Target row does not exist.
    pub const NOT_FOUND: &str = "notFound";
    /// Refused: would remove the last active owner.
    pub const LAST_OWNER: &str = "lastOwner";
    /// Claim ticket missing, expired, or already redeemed.
    pub const CLAIM_INVALID: &str = "claimInvalid";
    /// Local claim login needs a password; the ticket was not consumed.
    pub const PASSWORD_REQUIRED: &str = "passwordRequired";
    /// Same `operationId` reused with a different request body.
    pub const IDEMPOTENCY_CONFLICT: &str = "idempotencyConflict";
    /// Job admit found an equivalent pending/running row.
    pub const DUPLICATE: &str = "duplicate";
    /// Job admit refused because the pending+running cap was reached.
    pub const QUEUE_FULL: &str = "queueFull";
}

/// Named atomic library operation compiled by the host into a generic plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum DbAtomicParams {
    /// Delete a first-party user and personal data (last-owner guarded).
    #[serde(rename_all = "camelCase")]
    DeleteUser {
        /// `users.id` to delete.
        user_id: i64,
    },
    /// Set `users.status` (`active` / `disabled`; last-owner guarded).
    #[serde(rename_all = "camelCase")]
    SetUserStatus {
        /// `users.id` to update.
        user_id: i64,
        /// Canonical status string (`active` or `disabled`).
        status: String,
    },
    /// Set or clear the Argon2id password hash and bump `security_version`.
    #[serde(rename_all = "camelCase")]
    SetUserPasswordHash {
        /// `users.id` to update.
        user_id: i64,
        /// New hash, or `null` to clear.
        password_hash: Option<String>,
    },
    /// Set `users.role` (last-owner guarded on demotion).
    #[serde(rename_all = "camelCase")]
    SetUserRole {
        /// `users.id` to update.
        user_id: i64,
        /// Canonical role string (`owner` / `administrator` / `member`).
        role: String,
    },
    /// Consume a claim ticket, optionally set a first password, mint a session.
    #[serde(rename_all = "camelCase")]
    RedeemClaimTicket {
        /// SHA-256 hex digest of the claim ticket.
        token_hash: String,
        /// SHA-256 hex digest of the new portal session token.
        session_hash: String,
        /// RFC 3339 expiry for the minted session.
        expires_at: String,
        /// Raw User-Agent captured at session mint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_agent: Option<String>,
        /// Best-effort device class.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_type: Option<String>,
        /// Best-effort OS / client label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_label: Option<String>,
        /// Argon2id hash to set when the local user has none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_password_hash: Option<String>,
        /// Domain-separated HMAC of the invite password for idempotency.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password_fingerprint: Option<String>,
    },
    /// Consume a one-time OIDC RP state (`DELETE` + expiry check).
    #[serde(rename_all = "camelCase")]
    TakeOidcRpState {
        /// SHA-256 hex digest of the OAuth `state` parameter.
        state_hash: String,
    },
    /// Consume a one-time WebAuthn challenge (`DELETE` + expiry check).
    #[serde(rename_all = "camelCase")]
    TakeWebauthnChallenge {
        /// Public ceremony id returned to the browser.
        challenge_id: String,
        /// `register`, `login`, or `elevate`.
        kind: String,
    },
    /// Admit a durable job (dedup + pending cap + insert).
    #[serde(rename_all = "camelCase")]
    EnqueueJob {
        /// Job kind wire string (`scan`, `acquire`, `listen_sync`, `integration_scan`).
        kind: String,
        /// Versioned command envelope JSON.
        payload_json: String,
        /// Higher values are claimed first.
        priority: i64,
        /// Maximum claims before a failure is terminal.
        max_attempts: i64,
        /// Global cap on pending+running rows.
        max_pending: i64,
        /// Optional RFC 3339 delay before the job becomes claimable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_after: Option<String>,
    },
    /// Claim the next ready job in `resource_class` for `owner`.
    #[serde(rename_all = "camelCase")]
    ClaimNextJob {
        /// Resource class wire string (`network`, …).
        resource_class: String,
        /// Worker id stored as `lease_owner`.
        owner: String,
        /// Lease length in seconds.
        lease_secs: i64,
    },
    /// Reserve scratch-quota bytes for one job path.
    #[serde(rename_all = "camelCase")]
    ReserveJobTemp {
        /// Owning job id.
        job_id: String,
        /// Absolute filesystem path.
        path: String,
        /// Bytes to reserve for this path.
        reserved_bytes: i64,
        /// Global reserved-bytes cap.
        quota_bytes: i64,
    },
    /// Promote a sealed TOTP secret to `primary` and set `users.totp_enabled`.
    #[serde(rename_all = "camelCase")]
    ConfirmTotpEnrollment {
        /// `users.id` to enroll.
        user_id: i64,
        /// Payload format (`sealed-v1`).
        format: String,
        /// Sealed TOTP secret bytes (`b64:…`).
        ciphertext: String,
        /// Cipher algorithm identifier, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cipher_algorithm: Option<String>,
        /// AEAD nonce (`b64:…`), if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cipher_nonce: Option<String>,
        /// Legacy KDF algorithm, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kdf_algorithm: Option<String>,
        /// Legacy KDF salt (`b64:…`), if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kdf_salt: Option<String>,
        /// Legacy Argon2 memory cost in KiB, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kdf_m_cost: Option<i64>,
        /// Legacy Argon2 time cost, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kdf_t_cost: Option<i64>,
        /// Legacy Argon2 parallelism, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kdf_p_cost: Option<i64>,
        /// RFC 3339 timestamp to store as `created_at` on the primary row.
        created_at: String,
    },
    /// Delete TOTP secrets and clear `users.totp_enabled`.
    #[serde(rename_all = "camelCase")]
    DisableUserTotp {
        /// `users.id` to disable.
        user_id: i64,
    },
    /// Persist a domain event in the outbox (dedup on eventType+dedupKey).
    #[serde(rename_all = "camelCase")]
    PublishDomainEvent {
        /// Stable event id (UUID).
        id: String,
        /// Event type (`book_acquired`).
        event_type: String,
        /// Payload schema version.
        schema_version: i64,
        /// Tenant / account id.
        #[serde(default)]
        account_id: String,
        /// Producer plugin id; empty when unknown.
        #[serde(default)]
        source: String,
        /// Trace correlation id.
        #[serde(default)]
        correlation_id: String,
        /// Causing event or job id.
        #[serde(default)]
        causation_id: String,
        /// Unique with `eventType`.
        dedup_key: String,
        /// Bounded JSON payload.
        payload: String,
        /// FIFO key copied onto deliveries.
        #[serde(default)]
        ordering_key: String,
    },
    /// Update a book row and optionally publish `book_acquired` in the same batch.
    #[serde(rename_all = "camelCase")]
    SetAcquireStatus {
        /// Book UUID to update.
        book_uuid: String,
        /// Acquire status string (`acquired`, `downloading`, …).
        status: String,
        /// Object-storage key for the primary audio artifact.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        storage_key: Option<String>,
        /// Optional failure message.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
        /// Event id when publishing; empty skips the outbox insert.
        #[serde(default)]
        event_id: String,
        /// Event type when publishing (`book_acquired`).
        #[serde(default)]
        event_type: String,
        /// Payload schema version.
        #[serde(default)]
        schema_version: i64,
        /// Tenant / account id on the event.
        #[serde(default)]
        event_account_id: String,
        /// Producer plugin id; empty when unknown.
        #[serde(default)]
        source: String,
        /// Trace correlation id.
        #[serde(default)]
        correlation_id: String,
        /// Causing event or job id.
        #[serde(default)]
        causation_id: String,
        /// Unique with `eventType`.
        #[serde(default)]
        dedup_key: String,
        /// Bounded JSON payload.
        #[serde(default)]
        payload: String,
        /// FIFO key copied onto deliveries.
        #[serde(default)]
        ordering_key: String,
    },
    /// Create per-subscriber deliveries and mark the event dispatched.
    #[serde(rename_all = "camelCase")]
    DispatchEventDeliveries {
        /// Parent event id.
        event_id: String,
        /// JSON array of `{ "pluginId": "…" }` subscriber snapshots.
        subscribers_json: String,
        /// When true, this page is the last snapshot and may mark the parent dispatched.
        #[serde(default = "default_mark_dispatched")]
        mark_dispatched: bool,
    },
    /// Claim the next ready event delivery for `owner`.
    #[serde(rename_all = "camelCase")]
    ClaimNextEventDelivery {
        /// Worker id stored as `lease_owner`.
        owner: String,
        /// Lease length in seconds.
        lease_secs: i64,
        /// JSON array of plugin ids this worker can execute (`[]` claims nothing).
        #[serde(default)]
        plugin_ids_json: String,
        /// Cluster-wide max `running` deliveries per `(plugin_id, resource_class)`.
        #[serde(default = "default_claim_max_in_flight")]
        max_in_flight: i64,
    },
}

/// Serde default: one in-flight event delivery per (plugin, class).
fn default_claim_max_in_flight() -> i64 {
    1
}

/// Serde default: last dispatch page marks the parent event dispatched.
fn default_mark_dispatched() -> bool {
    true
}

/// Host-interpreted result of a named atomic operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DbAtomicResult {
    /// Application outcome (`ok`, `empty`, `notFound`, `lastOwner`, …).
    pub status: String,
    /// Op-specific record when `status` is `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<JsonValue>,
    /// Echo of the request `operationId`.
    #[serde(default)]
    pub operation_id: String,
    /// True when this result was loaded from a durable receipt.
    #[serde(default)]
    pub replayed: bool,
    /// RFC 3339 timestamp when the receipt was first written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_created_at: Option<String>,
    /// Handler/engine timing. Not hashed for idempotency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<DbAtomicTiming>,
}

impl DbAtomicResult {
    /// Successful operation with a JSON payload.
    #[must_use]
    pub fn ok(payload: JsonValue) -> Self {
        Self {
            status: atomic_status::OK.into(),
            payload: Some(payload),
            operation_id: String::new(),
            replayed: false,
            receipt_created_at: None,
            timing: None,
        }
    }

    /// Successful operation with no payload (`deleteUser`).
    #[must_use]
    pub fn ok_unit() -> Self {
        Self {
            status: atomic_status::OK.into(),
            payload: None,
            operation_id: String::new(),
            replayed: false,
            receipt_created_at: None,
            timing: None,
        }
    }

    /// Application failure or empty consume-once result (no payload).
    #[must_use]
    pub fn with_status(status: &str) -> Self {
        Self {
            status: status.to_string(),
            payload: None,
            operation_id: String::new(),
            replayed: false,
            receipt_created_at: None,
            timing: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_atomic_params_use_camel_case_op_tag() {
        let params = DbAtomicParams::DeleteUser { user_id: 7 };
        let v = serde_json::to_value(&params).unwrap();
        assert_eq!(v["op"], "deleteUser");
        assert_eq!(v["userId"], 7);
        let back: DbAtomicParams = serde_json::from_value(v).unwrap();
        assert_eq!(back, params);

        let take = DbAtomicParams::TakeOidcRpState {
            state_hash: "abc".into(),
        };
        let tv = serde_json::to_value(&take).unwrap();
        assert_eq!(tv["op"], "takeOidcRpState");
        assert_eq!(tv["stateHash"], "abc");

        let chal = DbAtomicParams::TakeWebauthnChallenge {
            challenge_id: "c1".into(),
            kind: "login".into(),
        };
        let cv = serde_json::to_value(&chal).unwrap();
        assert_eq!(cv["op"], "takeWebauthnChallenge");
        assert_eq!(cv["challengeId"], "c1");

        let disable = DbAtomicParams::DisableUserTotp { user_id: 9 };
        let dis_v = serde_json::to_value(&disable).unwrap();
        assert_eq!(dis_v["op"], "disableUserTotp");
        assert_eq!(dis_v["userId"], 9);
    }
}
