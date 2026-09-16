//! Bounded DTOs for the plugin ABI (never contain media bytes).

use serde::{Deserialize, Serialize};

use crate::generated::{
    Brand, CliSchema, ConfigOption, Entrypoint, PluginCapabilities, PortalAuthMode,
};
use crate::limits::{ScalarLimits, PRODUCT_API_VERSION};

/// Guest identity returned by `PluginWorker.describe` (the adapter merges the manifest projection over the author `describe()`).
///
/// Every field is a typed Cap'n Proto field of `PluginDescribe`; there is no
/// side-channel JSON. Storefront UI extras (`brand`, `config_options`,
/// `portal_auth_mode`, ...) are empty / default for kinds that do not use them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginDescribe {
    /// Must equal [`crate::limits::PRODUCT_API_VERSION`].
    pub api_version: u32,
    /// Plugin id matching `plugin.toml`.
    pub id: String,
    /// Optional UI name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Accepted RPC features (intersection with the host offer).
    #[serde(default)]
    pub rpc_features: Vec<String>,
    /// Effective numeric limits.
    pub scalar_limits: ScalarLimitsDto,
    /// Typed capability declaration (entrypoints, event consumers/producers,
    /// jobs, database and named bindings). The host compares it with the
    /// verified `plugin.toml` and the operator grant; widening is rejected.
    #[serde(default)]
    pub capabilities: PluginCapabilities,
    /// Portal Accounts connect mode for storefronts.
    #[serde(default)]
    pub portal_auth_mode: PortalAuthMode,
    /// Env var operators may set for password helpers (never required for
    /// Accounts UI connect).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_env_var: Option<String>,
    /// Alternate ids accepted for config / CLI targeting.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// UI sort weight among peers of the same kind; lower sorts first.
    #[serde(default)]
    pub sort_key: u32,
    /// Portal brand; `None` when the guest has no brand (wire `brand.id` is
    /// empty) and the host renders a neutral fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brand: Option<Brand>,
    /// Discoverable config option groups for source UIs.
    #[serde(default)]
    pub config_options: Vec<ConfigOption>,
    /// Embedded CLI schema (same shape as `cliDescribe`); empty when unused.
    #[serde(default)]
    pub cli: CliSchema,
}

impl Default for PluginDescribe {
    fn default() -> Self {
        Self {
            api_version: PRODUCT_API_VERSION,
            id: String::new(),
            display_name: None,
            rpc_features: Vec::new(),
            scalar_limits: ScalarLimits::default().into(),
            capabilities: PluginCapabilities::default(),
            portal_auth_mode: PortalAuthMode::Unspecified,
            password_env_var: None,
            aliases: Vec::new(),
            sort_key: 0,
            brand: None,
            config_options: Vec::new(),
            cli: CliSchema::default(),
        }
    }
}

impl PluginDescribe {
    /// True when the guest exports the named [`Entrypoint`].
    #[must_use]
    pub fn has_entrypoint(&self, entrypoint: Entrypoint) -> bool {
        self.capabilities.entrypoints.contains(&entrypoint)
    }

    /// True when the guest's default entrypoint consumes `event_type`.
    #[must_use]
    pub fn consumes_event(&self, event_type: &str) -> bool {
        self.capabilities
            .consumes
            .iter()
            .any(|c| c.event_type == event_type)
    }

    /// True when the guest's default entrypoint runs jobs of `job_type`.
    #[must_use]
    pub fn runs_job(&self, job_type: &str) -> bool {
        self.capabilities.jobs.iter().any(|j| j == job_type)
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
    #[serde(default, with = "crate::json_bytes::b64")]
    pub payload: Vec<u8>,
}

/// Media type of a JSON [`ExtensibleConfig`] payload.
pub const JSON_MEDIA_TYPE: &str = "application/json";

impl ExtensibleConfig {
    /// Wraps a JSON document as an `application/json` payload (schema version 1).
    #[must_use]
    pub fn json(value: &serde_json::Value) -> Self {
        Self {
            schema_version: 1,
            media_type: JSON_MEDIA_TYPE.into(),
            payload: serde_json::to_vec(value).unwrap_or_default(),
        }
    }

    /// Serializes `value` as an `application/json` payload (schema version 1).
    ///
    /// # Errors
    ///
    /// Returns [`crate::PluginErrorCode::Internal`] when `value` cannot be
    /// serialized.
    pub fn json_from<T: Serialize>(value: &T) -> crate::Result<Self> {
        let payload = serde_json::to_vec(value).map_err(|err| {
            crate::PluginError::internal(format!("config payload encode failed: {err}"))
        })?;
        Ok(Self {
            schema_version: 1,
            media_type: JSON_MEDIA_TYPE.into(),
            payload,
        })
    }

    /// True when no payload was supplied.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }

    /// Decodes the payload as a JSON document; an empty payload is `{}`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::PluginErrorCode::InvalidParams`] when the media type is
    /// not JSON or the bytes are not valid JSON.
    pub fn json_value(&self) -> crate::Result<serde_json::Value> {
        if self.payload.is_empty() {
            return Ok(serde_json::Value::Object(serde_json::Map::new()));
        }
        if !self.media_type.is_empty() && !is_json_media_type(&self.media_type) {
            return Err(crate::PluginError::invalid_params(format!(
                "config media type `{}` is not JSON",
                self.media_type
            )));
        }
        serde_json::from_slice(&self.payload).map_err(|err| {
            crate::PluginError::invalid_params(format!("config payload is not JSON: {err}"))
        })
    }

    /// Decodes the JSON payload into `T`; an empty payload decodes `{}`.
    ///
    /// # Errors
    ///
    /// Same as [`Self::json_value`], plus a deserialization failure into `T`.
    pub fn json_into<T: serde::de::DeserializeOwned>(&self) -> crate::Result<T> {
        let value = self.json_value()?;
        serde_json::from_value(value).map_err(|err| {
            crate::PluginError::invalid_params(format!("config payload decode failed: {err}"))
        })
    }
}

/// `application/json` and `application/*+json` structured-syntax suffixes.
fn is_json_media_type(media_type: &str) -> bool {
    // Strip any `; charset=...` parameters to compare the type/subtype essence.
    let essence = media_type
        .split_once(';')
        .map_or(media_type, |(essence, _params)| essence)
        .trim()
        .to_ascii_lowercase();
    essence == JSON_MEDIA_TYPE
        || (essence.starts_with("application/") && essence.ends_with("+json"))
}

/// Domain event (not a job). Outbox-produced, at-least-once, idempotent consume.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Producer plugin id; empty when unknown.
    #[serde(default)]
    pub source: String,
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
    #[serde(default, with = "crate::json_bytes::b64")]
    pub payload: Vec<u8>,
    /// Checkpoint JSON from a prior [`EventResult::Suspended`] (≤ [`MAX_CHECKPOINT_BYTES`]).
    #[serde(default)]
    pub checkpoint_json: String,
    /// Schema version of [`Self::checkpoint_json`].
    #[serde(default)]
    pub checkpoint_schema_version: u32,
    /// Resume ordinal copied from the delivery row.
    #[serde(default)]
    pub invocation_sequence: u32,
    /// True when this invocation continues a prior `suspended` result.
    #[serde(default)]
    pub resume_pending: bool,
}

/// Per-event result of [`crate::EventConsumer::event`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum EventResult {
    /// Event processed.
    Ack,
    /// Transient failure; retry after `retry_at_unix_ms`.
    #[serde(rename_all = "camelCase")]
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
    /// Durable sleep: persist checkpoint, release the process, resume later.
    #[serde(rename_all = "camelCase")]
    Suspended {
        /// Bounded checkpoint JSON (must be ≤ [`MAX_CHECKPOINT_BYTES`]).
        #[serde(default)]
        checkpoint_json: String,
        /// Checkpoint schema version.
        #[serde(default)]
        checkpoint_schema_version: u32,
        /// UTC unix-ms wake hint.
        wake_at_unix_ms: u64,
        /// Event type that can wake this sleep; empty = timestamp-only.
        #[serde(default)]
        wake_on_event_type: String,
        /// Host-owned payload object filter JSON; empty = type only.
        #[serde(default)]
        wake_on_filter_json: String,
    },
}

impl EventResult {
    /// Decode JSON `{"kind":"ack"|…}`. Missing or unknown `kind` is an error.
    ///
    /// # Errors
    ///
    /// Returns [`crate::PluginErrorCode::InvalidParams`] when the payload is not
    /// a tagged [`EventResult`], or [`crate::PluginErrorCode::PayloadTooLarge`]
    /// when a `suspended` checkpoint exceeds [`MAX_CHECKPOINT_BYTES`].
    pub fn from_json_value(value: &serde_json::Value) -> crate::Result<Self> {
        let parsed: Self = serde_json::from_value(value.clone()).map_err(|err| {
            crate::PluginError::invalid_params(format!("malformed EventResult: {err}"))
        })?;
        parsed.reject_oversized_checkpoint()
    }

    /// Decode a JSON object string as [`EventResult`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::from_json_value`].
    pub fn from_json_str(raw: &str) -> crate::Result<Self> {
        let value: serde_json::Value = serde_json::from_str(raw).map_err(|err| {
            crate::PluginError::invalid_params(format!("malformed EventResult: {err}"))
        })?;
        Self::from_json_value(&value)
    }

    /// Reject a `suspended` result whose checkpoint exceeds [`MAX_CHECKPOINT_BYTES`].
    ///
    /// # Errors
    ///
    /// Returns [`crate::PluginErrorCode::PayloadTooLarge`] when `checkpoint_json`
    /// is longer than [`MAX_CHECKPOINT_BYTES`].
    fn reject_oversized_checkpoint(self) -> crate::Result<Self> {
        if let Self::Suspended {
            checkpoint_json, ..
        } = &self
        {
            if checkpoint_json.len() > MAX_CHECKPOINT_BYTES as usize {
                return Err(crate::PluginError::payload_too_large(format!(
                    "checkpoint of {} bytes exceeds {MAX_CHECKPOINT_BYTES}",
                    checkpoint_json.len()
                )));
            }
        }
        Ok(self)
    }
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

/// Bounded query page for host-internal SeaORM session helpers (not Cap'n ABI).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryPage {
    /// JSON-encoded rows.
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

/// Maximum checkpoint payload size (bytes).
pub const MAX_CHECKPOINT_BYTES: u32 = crate::plugin_capnp::MAX_CHECKPOINT_BYTES;

/// Versioned durable command envelope (not a domain event).
///
/// Idempotency keys are scoped to `(account, plugin, commandType)` until a
/// terminal fenced outcome is committed. They are not reusable across accounts.
/// `deadline_unix_ms` is a guest hint; the host fence/lease is authoritative
/// and must not be outlived (clock skew across VPS nodes).
/// The envelope is not independently persisted as opaque bytes across ABI
/// majors; command `payload_schema_version` and checkpoint schema versions
/// version the stored pieces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobInvocation {
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

/// Outcome of [`crate::JobRunner::job`].
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::json_bytes::opt_b64"
    )]
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::json_bytes::opt_b64"
    )]
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::json_bytes::opt_b64"
    )]
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

    #[test]
    fn event_result_serde_includes_suspended() {
        let suspended = EventResult::Suspended {
            checkpoint_json: r#"{"n":1}"#.into(),
            checkpoint_schema_version: 1,
            wake_at_unix_ms: 42,
            wake_on_event_type: String::new(),
            wake_on_filter_json: String::new(),
        };
        let json = serde_json::to_value(&suspended).unwrap();
        assert_eq!(json["kind"], "suspended");
        assert_eq!(json["wakeAtUnixMs"], 42);
        let back: EventResult = serde_json::from_value(json).unwrap();
        assert_eq!(back, suspended);
        let legacy = EventResult::from_json_str(
            r#"{"kind":"suspended","checkpointJson":"{}","checkpointSchemaVersion":1,"wakeAtUnixMs":7}"#,
        )
        .unwrap();
        assert_eq!(
            legacy,
            EventResult::Suspended {
                checkpoint_json: "{}".into(),
                checkpoint_schema_version: 1,
                wake_at_unix_ms: 7,
                wake_on_event_type: String::new(),
                wake_on_filter_json: String::new(),
            }
        );
    }

    #[test]
    fn event_result_from_json_rejects_missing_and_unknown_kind() {
        assert!(EventResult::from_json_str("{}").is_err());
        assert!(EventResult::from_json_str("").is_err());
        assert!(EventResult::from_json_str("not-json").is_err());
        assert!(EventResult::from_json_str(r#"{"kind":"nope"}"#).is_err());
        assert_eq!(
            EventResult::from_json_str(r#"{"kind":"ack"}"#).unwrap(),
            EventResult::Ack
        );
    }
}
