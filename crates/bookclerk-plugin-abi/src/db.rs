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

/// Result of a successful [`crate::methods::db_connect`].
///
/// Tells the host which SeaORM dialect to use when composing subsequent
/// `dbQuery` / `dbExecute` statements against this guest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DbConnectResult {
    /// SeaORM dialect string the host should use for the RPC proxy
    /// (`"sqlite"` or `"postgres"`; D1 guests report `"sqlite"`).
    pub dialect: String,
}

impl DbConnectResult {
    /// Connect result advertising the SQLite (or D1) dialect.
    #[must_use]
    pub fn sqlite() -> Self {
        Self {
            dialect: String::from("sqlite"),
        }
    }

    /// Connect result advertising the Postgres dialect.
    #[must_use]
    pub fn postgres() -> Self {
        Self {
            dialect: String::from("postgres"),
        }
    }
}
