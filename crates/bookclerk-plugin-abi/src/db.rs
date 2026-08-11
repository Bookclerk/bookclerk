//! Database plugin Workers RPC DTOs.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// SQL + bind parameters crossing the host↔database-guest boundary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StatementDto {
    pub sql: String,
    #[serde(default)]
    pub values: Vec<JsonValue>,
}

/// One result row from `dbQuery`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProxyRowDto {
    pub values: BTreeMap<String, JsonValue>,
}

/// Result of `dbQuery`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct QueryResultDto {
    pub rows: Vec<ProxyRowDto>,
}

/// Result of `dbExecute`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExecResultDto {
    pub last_insert_id: u64,
    pub rows_affected: u64,
}

/// Params for `dbConnect` (tagged by backend).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "backend", rename_all = "lowercase")]
pub enum DbConnectParams {
    #[serde(rename_all = "camelCase")]
    Sqlite {
        plugin_data_dir: String,
        /// Host fallback path when no fd side channel is wired.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sqlite_path: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    D1 {
        plugin_data_dir: String,
        account_id: String,
        database_id: String,
        api_base: String,
        api_token: String,
    },
    #[serde(rename_all = "camelCase")]
    Postgres {
        plugin_data_dir: String,
        url: String,
    },
}

/// Result of a successful `dbConnect`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DbConnectResult {
    /// SeaORM dialect the host should use for the RPC proxy (`sqlite` or `postgres`).
    pub dialect: String,
}

impl DbConnectResult {
    #[must_use]
    pub fn sqlite() -> Self {
        Self {
            dialect: String::from("sqlite"),
        }
    }

    #[must_use]
    pub fn postgres() -> Self {
        Self {
            dialect: String::from("postgres"),
        }
    }
}
