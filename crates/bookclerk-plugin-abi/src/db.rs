//! Database plugin Workers RPC DTOs (`kind = "database"`).
//!
//! Guests such as `sqlite` / `d1` / `postgres` implement the SeaORM proxy
//! boundary. The host never links SQL engines; it opens the library through
//! these RPC methods after [`crate::methods::db_connect`].
//!
//! | Method | Params | Result |
//! | --- | --- | --- |
//! | [`crate::methods::db_connect`] | [`DbConnectParams`] | [`DbConnectResult`] |
//! | [`crate::methods::db_ping`] | (none) | success / [`crate::PluginError`] |
//! | [`crate::methods::db_query`] | [`StatementDto`] | [`QueryResultDto`] |
//! | [`crate::methods::db_execute`] | [`StatementDto`] | [`ExecResultDto`] |
//! | [`crate::methods::db_begin`] | [`DbBeginParams`] | [`DbBeginResult`] |
//! | [`crate::methods::db_commit`] | [`DbTxnParams`] | success / [`crate::PluginError`] |
//! | [`crate::methods::db_rollback`] | [`DbTxnParams`] | success / [`crate::PluginError`] |
//! | [`crate::methods::db_atomic`] | [`DbAtomicRequest`] | [`DbAtomicResult`] |
//!
//! Wire fields use camelCase. The `backend` tag on [`DbConnectParams`] is
//! lowercase (`sqlite`, `d1`, `postgres`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// SQL statement plus bind parameters crossing the host↔database-guest boundary.
///
/// Used as params for both [`crate::methods::db_query`] and
/// [`crate::methods::db_execute`]. Bind values are JSON (null, bool, number,
/// string, or nested arrays) matching SeaORM's RPC proxy encoding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StatementDto {
    /// SQL text with positional or named placeholders as understood by the
    /// guest dialect (SQLite `?`, Postgres `$1`, …).
    pub sql: String,
    /// Ordered bind values for the statement (wire `values`; default empty).
    #[serde(default)]
    pub values: Vec<JsonValue>,
    /// Guest transaction id from [`crate::methods::db_begin`] (wire `txnId`).
    ///
    /// Omitted for autocommit statements. When set, the guest runs the
    /// statement inside that transaction (or nested savepoint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub txn_id: Option<String>,
}

/// Params for [`crate::methods::db_begin`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DbBeginParams {
    /// Existing transaction to nest a savepoint under (wire `parentTxnId`).
    ///
    /// Omitted to start a top-level transaction. The guest serializes
    /// top-level begins so SQLite / D1 never interleave writers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_txn_id: Option<String>,
}

/// Result of a successful [`crate::methods::db_begin`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DbBeginResult {
    /// Opaque id the host must send on subsequent statements and
    /// commit/rollback (wire `txnId`).
    pub txn_id: String,
}

/// Params for [`crate::methods::db_commit`] and [`crate::methods::db_rollback`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DbTxnParams {
    /// Transaction id returned by [`crate::methods::db_begin`] (wire `txnId`).
    pub txn_id: String,
}

/// One result row from [`crate::methods::db_query`].
///
/// Column names are the keys the guest returns (typically the SQL alias or
/// table column name); values are JSON-encoded cell data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProxyRowDto {
    /// Column name → JSON cell value map for this row (wire `values`).
    pub values: BTreeMap<String, JsonValue>,
}

/// Successful result of [`crate::methods::db_query`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct QueryResultDto {
    /// Zero or more rows in result-set order.
    pub rows: Vec<ProxyRowDto>,
}

/// Successful result of [`crate::methods::db_execute`] (INSERT/UPDATE/DELETE).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExecResultDto {
    /// Last auto-increment / identity value when the backend provides one
    /// (wire `lastInsertId`); `0` when not applicable.
    pub last_insert_id: u64,
    /// Number of rows affected by the statement (wire `rowsAffected`).
    pub rows_affected: u64,
}

/// Tagged connect params for [`crate::methods::db_connect`].
///
/// Discriminant is wire field `backend` with lowercase tags. SQLite guests
/// typically receive `library.db` on the side-channel FD at connect time;
/// D1 / Postgres receive host-injected credentials in the params.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "backend", rename_all = "lowercase")]
pub enum DbConnectParams {
    /// Local SQLite file backend (`backend: "sqlite"`).
    #[serde(rename_all = "camelCase")]
    Sqlite {
        /// Scoped writable directory for this plugin
        /// (`…/plugins/<id>/data`, wire `pluginDataDir`).
        plugin_data_dir: String,
        /// Host fallback absolute path to the DB file when no FD side channel
        /// is wired (unconfined / best-effort). Omitted when FD 3 carries the
        /// open file (wire `sqlitePath`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sqlite_path: Option<String>,
    },
    /// Cloudflare D1 HTTP API backend (`backend: "d1"`).
    #[serde(rename_all = "camelCase")]
    D1 {
        /// Scoped writable directory for this plugin (wire `pluginDataDir`).
        plugin_data_dir: String,
        /// Cloudflare account id for the D1 API (wire `accountId`).
        account_id: String,
        /// D1 database UUID (wire `databaseId`).
        database_id: String,
        /// API base URL (for example `https://api.cloudflare.com/client/v4`).
        api_base: String,
        /// Bearer / API token the host injects; guests must not read env for this.
        api_token: String,
    },
    /// PostgreSQL connection-string backend (`backend: "postgres"`).
    #[serde(rename_all = "camelCase")]
    Postgres {
        /// Scoped writable directory for this plugin (wire `pluginDataDir`).
        plugin_data_dir: String,
        /// Full Postgres connection URL (host-injected; may contain secrets).
        url: String,
    },
}

fn default_true() -> bool {
    true
}

/// Result of a successful [`crate::methods::db_connect`].
///
/// Tells the host which SeaORM dialect to use when composing subsequent
/// `dbQuery` / `dbExecute` statements against this guest, and whether
/// interactive `dbBegin` is available.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DbConnectResult {
    /// SeaORM dialect string the host should use for the RPC proxy
    /// (`"sqlite"` or `"postgres"`; D1 guests report `"sqlite"`).
    pub dialect: String,
    /// When `false`, the host must not use SeaORM `begin()` / `dbBegin`.
    ///
    /// SQLite and Postgres default to `true`. D1 HTTP cannot keep `BEGIN`
    /// open across RPCs; those guests set `false` and implement
    /// [`crate::methods::db_atomic`] instead. Omitted on the wire by older
    /// guests (treated as `true`).
    #[serde(default = "default_true")]
    pub interactive_txn: bool,
}

impl DbConnectResult {
    /// Connect result advertising the SQLite dialect with interactive transactions.
    #[must_use]
    pub fn sqlite() -> Self {
        Self {
            dialect: String::from("sqlite"),
            interactive_txn: true,
        }
    }

    /// Connect result advertising the Postgres dialect with interactive transactions.
    #[must_use]
    pub fn postgres() -> Self {
        Self {
            dialect: String::from("postgres"),
            interactive_txn: true,
        }
    }

    /// Connect result for Cloudflare D1 (SQLite dialect, no interactive `BEGIN`).
    #[must_use]
    pub fn d1() -> Self {
        Self {
            dialect: String::from("sqlite"),
            interactive_txn: false,
        }
    }
}

/// Application status strings returned by [`crate::methods::db_atomic`].
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
}

/// Named atomic library operation for [`crate::methods::db_atomic`].
///
/// D1 guests run each variant as one HTTP `batch()` (one SQL transaction)
/// with control flow encoded in `WHERE` clauses so the host does not need
/// mid-transaction reads. Consume-once variants use `DELETE … RETURNING`.
/// SQLite and Postgres guests run the same command in a native local
/// transaction. Every backend writes a durable `operationId` receipt.
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
        ///
        /// Argon2id salts change on every POST; this fingerprint is stable across
        /// retries and is not stored as the password hash.
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
}

/// Host-generated idempotency envelope for [`crate::methods::db_atomic`].
///
/// `operation` is the named command. `operation_id` keys a durable receipt so
/// a committed batch whose HTTP/RPC response is lost can be retried without
/// a second mutation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DbAtomicRequest {
    /// Caller-chosen idempotency key (UUID). Retries must reuse the same id.
    pub operation_id: String,
    /// Named library operation to run (or replay from a receipt).
    pub operation: DbAtomicParams,
}

/// Optional engine timing for [`DbAtomicResult`]. Not part of the idempotency hash.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct DbAtomicTiming {
    /// Monotonic duration of this plugin-handler attempt.
    pub attempt_elapsed_us: u64,
    /// Engine-reported SQL/transaction time when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_execution_us: Option<u64>,
    /// How `db_execution_us` was measured (`d1_sql_duration`, `sqlite_txn`, `postgres_txn`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_timing_source: Option<String>,
}

/// Result of a successful [`crate::methods::db_atomic`] RPC.
///
/// `status` is an application outcome ([`atomic_status`]); SQL failures are
/// plugin errors and roll back the D1 batch. `payload` is a library record
/// JSON object using snake_case field names matching `UserRecord` /
/// `PortalIdentity`. Receipt metadata lets the host replay a committed
/// operation after an ambiguous transport error.
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
