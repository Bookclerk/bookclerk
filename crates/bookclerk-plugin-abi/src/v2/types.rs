//! Bounded DTOs for ABI v2 (never contain media bytes).

use serde::{Deserialize, Serialize};

use super::limits::{ScalarLimits, ABI_MAJOR, ABI_MINOR, PRODUCT_API_VERSION};

/// Guest identity returned by `BookclerkPlugin.describe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginDescribe {
    /// Must equal [`super::PRODUCT_API_VERSION`].
    pub api_version: u32,
    /// Plugin id matching `plugin.toml`.
    pub id: String,
    /// `source` / `integration` / `output` / `database`.
    pub kind: String,
    /// Optional UI name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Accepted RPC features (intersection with the host offer).
    #[serde(default)]
    pub rpc_features: Vec<String>,
    /// Effective numeric limits.
    pub scalar_limits: ScalarLimitsDto,
    /// Major ABI number (`apiVersion`). Spawn rejects a mismatch.
    #[serde(default)]
    pub abi_major: u32,
    /// Minor ABI number. Hosts ignore unknown optional fields.
    #[serde(default)]
    pub abi_minor: u32,
    /// Advertised factories. Host intersects with the signed manifest allowlist.
    #[serde(default)]
    pub supported_roles: Vec<String>,
    /// Handshake-era extras (brand, CLI, method names). Bounded JSON.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub metadata_json: String,
}

impl Default for PluginDescribe {
    fn default() -> Self {
        Self {
            api_version: PRODUCT_API_VERSION,
            id: String::new(),
            kind: String::new(),
            display_name: None,
            rpc_features: Vec::new(),
            scalar_limits: ScalarLimits::default().into(),
            abi_major: ABI_MAJOR,
            abi_minor: ABI_MINOR,
            supported_roles: Vec::new(),
            metadata_json: String::new(),
        }
    }
}

/// Versioned plugin-specific config (not a substitute for typed ABI fields).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensibleConfig {
    /// Payload schema version.
    pub schema_version: u32,
    /// IANA media type or `application/json`.
    #[serde(default)]
    pub media_type: String,
    /// Bounded payload bytes.
    #[serde(default)]
    pub payload: Vec<u8>,
}

/// Domain event (not a job). Outbox-produced, at-least-once, idempotent consume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DomainEvent {
    /// Unique event id.
    pub event_id: String,
    /// Event type name.
    pub event_type: String,
    /// Payload schema version.
    pub schema_version: u32,
    /// UTC unix milliseconds.
    pub occurred_at_unix_ms: u64,
    /// Tenant / account id.
    #[serde(default)]
    pub account_id: String,
    /// Trace correlation id.
    #[serde(default)]
    pub correlation_id: String,
    /// Prior event or invocation that caused this one.
    #[serde(default)]
    pub causation_id: String,
    /// Idempotent consume key.
    #[serde(default)]
    pub deduplication_key: String,
    /// 1-based delivery attempt.
    pub delivery_attempt: u32,
    /// Bounded payload.
    #[serde(default)]
    pub payload: Vec<u8>,
}

/// Result of [`crate::v2::Integration::on_event`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum EventResult {
    /// Event processed.
    Ack,
    /// Transient failure; retry after `retry_at_unix_ms`.
    Retry {
        /// UTC unix-ms hint.
        retry_at_unix_ms: u64,
        /// Operator-facing reason.
        #[serde(default)]
        reason: String,
    },
    /// Permanent rejection.
    Reject {
        /// Operator-facing reason.
        #[serde(default)]
        reason: String,
    },
    /// Poison; do not redeliver.
    DeadLetter {
        /// Operator-facing reason.
        #[serde(default)]
        reason: String,
    },
}

/// Health payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthOk {
    /// True when the role is ready.
    pub ok: bool,
    /// Operator-facing detail.
    #[serde(default)]
    pub detail: String,
}

/// Database statement (typed ABI; not opaque `StatementDto` JSON).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Statement {
    /// SQL text.
    pub sql: String,
    /// JSON-encoded parameter values (migration bridge).
    #[serde(default)]
    pub values_json: String,
}

/// Execute result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecResult {
    /// Last insert id when the engine provides one.
    pub last_insert_id: i64,
    /// Rows affected.
    pub rows_affected: u64,
}

/// Bounded query page. Never requires materializing an unbounded result set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryPage {
    /// JSON-encoded rows (migration bridge).
    #[serde(default)]
    pub rows_json: String,
    /// Continuation token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Wire form of [`ScalarLimits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScalarLimitsDto {
    /// Max ordinary RPC value size in bytes.
    pub max_scalar_bytes: u32,
    /// Max `ByteSource.pull` window in bytes.
    pub max_stream_window_bytes: u32,
    /// Max objects per list page.
    pub max_list_page: u32,
}

impl From<ScalarLimits> for ScalarLimitsDto {
    fn from(value: ScalarLimits) -> Self {
        Self {
            max_scalar_bytes: value.max_scalar_bytes,
            max_stream_window_bytes: value.max_stream_window_bytes,
            max_list_page: value.max_list_page,
        }
    }
}

impl From<ScalarLimitsDto> for ScalarLimits {
    fn from(value: ScalarLimitsDto) -> Self {
        Self {
            max_scalar_bytes: value.max_scalar_bytes,
            max_stream_window_bytes: value.max_stream_window_bytes,
            max_list_page: value.max_list_page,
        }
    }
}

/// Injected destination knobs. Opaque JSON only — no OS paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DestinationContext {
    /// Opaque JSON (bucket, root, credentials, …). Host-private jail layout
    /// stays off this struct.
    #[serde(default)]
    pub json: String,
}

/// Injected source knobs. Opaque JSON only — no OS paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceContext {
    /// Opaque JSON (credentials, marketplace, …).
    #[serde(default)]
    pub json: String,
}

/// Job worker instantiation knobs. Opaque JSON only — no OS paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerContext {
    /// Durable job id.
    #[serde(default)]
    pub job_id: String,
    /// Opaque JSON extras.
    #[serde(default)]
    pub json: String,
}

/// Current envelope schema version for [`JobInvocation`].
pub const ENVELOPE_VERSION: u32 = 1;

/// Maximum checkpoint payload size (bytes).
pub const MAX_CHECKPOINT_BYTES: u32 = 65_536;

/// Versioned durable command envelope (not a domain event).
///
/// Idempotency keys are scoped to `(account, plugin, commandType)` until a
/// terminal fenced outcome is committed. They are not reusable across accounts.
/// `deadline_unix_ms` is a guest hint; the host fence/lease is authoritative
/// and must not be outlived (clock skew across VPS nodes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobInvocation {
    /// Envelope schema version (must equal [`ENVELOPE_VERSION`]).
    pub envelope_version: u32,
    /// Command payload schema version for [`Self::command_type`].
    pub payload_schema_version: u32,
    /// Unique id for this attempt envelope.
    pub invocation_id: String,
    /// Command type (`stream_copy`, …).
    pub command_type: String,
    /// JSON payload for that command type.
    #[serde(default)]
    pub payload_json: String,
    /// Idempotency key (see struct docs for scope).
    pub idempotency_key: String,
    /// 1-based attempt counter.
    pub attempt: u32,
    /// Correlation id for traces.
    #[serde(default)]
    pub correlation_id: String,
    /// Optional causation id (prior invocation that produced this one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    /// Guest-visible UTC deadline as unix milliseconds.
    pub deadline_unix_ms: u64,
    /// Bounded, versioned checkpoint from a prior suspension.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<JobCheckpoint>,
    /// Resume ordinal; distinct from failure [`Self::attempt`].
    #[serde(default)]
    pub invocation_sequence: u32,
    /// Deterministic step identifier within this invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
}

/// Claimed-lease fields used to populate a [`JobInvocation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobInvocationLease {
    /// Durable job id (`jobs.id`).
    pub job_id: String,
    /// 1-based attempt counter from the claim.
    pub attempt: u32,
    /// Lease generation from the successful claim.
    pub generation: i64,
    /// Queue dedup key; stable across reclaim of the same command.
    pub dedup_key: String,
    /// Lease expiry as UTC unix milliseconds (guest deadline hint).
    pub deadline_unix_ms: u64,
    /// Checkpoint restored from a prior [`JobOutcome::Suspended`].
    pub checkpoint: Option<JobCheckpoint>,
    /// Resume ordinal from the durable payload; distinct from [`Self::attempt`].
    pub invocation_sequence: u32,
}

/// Bounded, versioned checkpoint attached to an invocation or suspension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobCheckpoint {
    /// Checkpoint payload schema version. Unknown versions fail closed.
    pub schema_version: u32,
    /// JSON checkpoint bytes (must be ≤ [`MAX_CHECKPOINT_BYTES`]).
    #[serde(default)]
    pub json: String,
}

impl JobInvocation {
    /// Builds a v1 `stream_copy` invocation with a far-future deadline.
    ///
    /// Tests and accountless helpers use this. Production copy jobs should call
    /// [`Self::stream_copy_from_lease`] so attempt, deadline, and idempotency
    /// come from the claimed fence.
    #[must_use]
    pub fn stream_copy(job_id: impl Into<String>, payload_json: impl Into<String>) -> Self {
        let job_id = job_id.into();
        Self::stream_copy_from_lease(
            JobInvocationLease {
                job_id: job_id.clone(),
                attempt: 1,
                generation: 1,
                dedup_key: job_id,
                deadline_unix_ms: u64::MAX / 2,
                checkpoint: None,
                invocation_sequence: 1,
            },
            payload_json,
        )
    }

    /// Builds a `stream_copy` envelope from a claimed job lease.
    ///
    /// `invocation_id` is unique per attempt (`job_id:attempt:generation`).
    /// `idempotency_key` is the durable dedup key and stays stable across
    /// reclaim. `invocation_sequence` is the resume ordinal from the lease
    /// (distinct from failure `attempt`). `deadline_unix_ms` is the lease
    /// expiry as unix milliseconds.
    #[must_use]
    pub fn stream_copy_from_lease(
        lease: JobInvocationLease,
        payload_json: impl Into<String>,
    ) -> Self {
        let attempt = lease.attempt.max(1);
        Self {
            envelope_version: ENVELOPE_VERSION,
            payload_schema_version: 1,
            invocation_id: format!("{}:{attempt}:{}", lease.job_id, lease.generation),
            command_type: "stream_copy".into(),
            payload_json: payload_json.into(),
            idempotency_key: lease.dedup_key,
            attempt,
            correlation_id: lease.job_id,
            causation_id: None,
            deadline_unix_ms: lease.deadline_unix_ms,
            checkpoint: lease.checkpoint,
            invocation_sequence: lease.invocation_sequence.max(1),
            step_id: None,
        }
    }
}

/// Outcome of [`crate::v2::JobHandler::handle`].
///
/// Suspension is durable only after Bookclerk atomically commits the fenced
/// outcome. Open streams and process memory do not survive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum JobOutcome {
    /// Handler finished successfully.
    Completed {
        /// Operator-facing summary.
        #[serde(default)]
        message: String,
        /// Bytes copied when this was a stream copy.
        #[serde(default)]
        bytes_copied: u64,
    },
    /// Transient failure; host may retry after `retry_after_unix_ms`.
    Retryable {
        /// Operator-facing summary.
        #[serde(default)]
        message: String,
        /// Optional UTC unix-ms hint for the next attempt.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_unix_ms: Option<u64>,
    },
    /// Permanent rejection (do not retry the same payload).
    Rejected {
        /// Operator-facing summary.
        #[serde(default)]
        message: String,
    },
    /// Cancelled by host fence / guest abort.
    Cancelled {
        /// Operator-facing summary.
        #[serde(default)]
        message: String,
    },
    /// Durable pause; resume with `checkpoint` after `wake_at_unix_ms`.
    Suspended {
        /// Checkpoint to restore on wake.
        checkpoint: JobCheckpoint,
        /// UTC unix-ms wake hint.
        wake_at_unix_ms: u64,
    },
}

impl JobOutcome {
    /// True when the host should treat the invocation as finished.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            JobOutcome::Completed { .. }
                | JobOutcome::Rejected { .. }
                | JobOutcome::Cancelled { .. }
        )
    }
}

/// Object listing entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectInfo {
    /// Object key.
    pub key: String,
    /// Size in bytes.
    pub size: u64,
}

/// Metadata without a body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectMetadata {
    /// Object key.
    pub key: String,
    /// Size in bytes.
    pub size: u64,
    /// MIME type when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Opaque etag when the backend provides one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// SHA-256 digest when computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<Vec<u8>>,
}

/// Paginated list request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListOptions {
    /// Key prefix filter.
    #[serde(default)]
    pub prefix: String,
    /// Opaque continuation token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Requested page size (0 = default).
    #[serde(default)]
    pub limit: u32,
}

/// One page of keys.
///
/// Cursors are opaque. Missing/stale cursors must return `invalid_cursor`,
/// never silently restart at page one. Concurrent mutation is weakly
/// consistent unless a backend snapshot is documented.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListPage {
    /// Objects in this page.
    pub objects: Vec<ObjectInfo>,
    /// Continuation token; `None` when this is the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Write options for `Destination.put`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteOptions {
    /// MIME type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Known length; omit when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_length: Option<u64>,
    /// Expected SHA-256 of the body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<Vec<u8>>,
    /// Destination-side stage-and-publish token. Empty means a one-shot put.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_token: Option<String>,
    /// When true, `put` stages remotely and does not publish until `commit`.
    #[serde(default)]
    pub stage_only: bool,
}

/// Result of a streamed put.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PutResult {
    /// Object key written.
    pub key: String,
    /// Bytes accepted.
    pub bytes_written: u64,
    /// Backend etag when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// Digest of the written body when computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<Vec<u8>>,
}

/// Result of a server-side copy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyResult {
    /// Bytes copied when known.
    pub bytes_copied: u64,
}

/// Plugin-declared Bookclerk-as-IdP relying-party template.
///
/// The host materializes `oidc_clients` rows. Plugins never mint tokens.
/// `origin_config_key` is a dotted config path such as
/// `integrations.audiobookshelf.base_url`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OidcClientTemplate {
    /// Stable OAuth `client_id` registered with Bookclerk's authorization server.
    pub client_id: String,
    /// Operator-facing card title.
    #[serde(default)]
    pub display_name: String,
    /// Path appended to the plugin origin (e.g. `/auth/openid/callback`).
    pub callback_path: String,
    /// When true, the client is public PKCE (no secret).
    #[serde(default = "default_true")]
    pub public_client: bool,
    /// Scopes offered on first materialization (`openid` / `profile` typical).
    #[serde(default)]
    pub default_scopes: Vec<String>,
    /// When true, `/oidc/token` may issue refresh tokens for new rows.
    #[serde(default = "default_true")]
    pub issue_refresh_token: bool,
    /// Dotted config key that supplies the player origin.
    pub origin_config_key: String,
}

/// Serde default for `public_client` and `issue_refresh_token`.
fn default_true() -> bool {
    true
}

impl OidcClientTemplate {
    /// Scopes used when the guest omitted `defaultScopes`.
    #[must_use]
    pub fn scopes_or_default(&self) -> Vec<String> {
        if self.default_scopes.is_empty() {
            vec!["openid".into(), "profile".into()]
        } else {
            self.default_scopes.clone()
        }
    }

    /// Card title, falling back to `client_id`.
    #[must_use]
    pub fn display_name_or_id(&self) -> &str {
        if self.display_name.trim().is_empty() {
            self.client_id.as_str()
        } else {
            self.display_name.as_str()
        }
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[test]
    fn lease_attempts_are_distinct_and_share_idempotency_key() {
        let first = JobInvocation::stream_copy_from_lease(
            JobInvocationLease {
                job_id: "copy-1".into(),
                attempt: 1,
                generation: 3,
                dedup_key: "dedup-copy-1".into(),
                deadline_unix_ms: 1_000,
                checkpoint: None,
                invocation_sequence: 1,
            },
            "{}",
        );
        let second = JobInvocation::stream_copy_from_lease(
            JobInvocationLease {
                job_id: "copy-1".into(),
                attempt: 2,
                generation: 4,
                dedup_key: "dedup-copy-1".into(),
                deadline_unix_ms: 2_000,
                checkpoint: Some(JobCheckpoint {
                    schema_version: 1,
                    json: "{\"n\":1}".into(),
                }),
                invocation_sequence: 1,
            },
            "{}",
        );
        assert_ne!(first.invocation_id, second.invocation_id);
        assert_eq!(first.idempotency_key, second.idempotency_key);
        assert_eq!(first.attempt, 1);
        assert_eq!(second.attempt, 2);
        assert_eq!(second.deadline_unix_ms, 2_000);
        assert_eq!(
            second.checkpoint.as_ref().map(|c| c.json.as_str()),
            Some("{\"n\":1}")
        );
    }

    #[test]
    fn invocation_sequence_is_not_collapsed_to_attempt() {
        let invocation = JobInvocation::stream_copy_from_lease(
            JobInvocationLease {
                job_id: "copy-1".into(),
                attempt: 1,
                generation: 3,
                dedup_key: "dedup-copy-1".into(),
                deadline_unix_ms: 1_000,
                checkpoint: None,
                invocation_sequence: 4,
            },
            "{}",
        );
        assert_eq!(invocation.attempt, 1);
        assert_eq!(invocation.invocation_sequence, 4);
    }
}
