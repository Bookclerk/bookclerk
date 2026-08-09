//! Cloudflare D1 HTTP API proxy for SeaORM.

use std::collections::BTreeMap;
use std::sync::Mutex;

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use bookclerk_library::{b64_string_to_bytes, bytes_to_b64_string, LibraryError, Result};
use sea_orm::{
    Database, DatabaseConnection, DbBackend, DbErr, ProxyDatabaseTrait, ProxyExecResult, ProxyRow,
    Statement, Value,
};
use serde_json::{json, Value as JsonValue};

/// Open Cloudflare D1 with an explicit API token (host-mediated).
pub async fn open(
    api_base: String,
    account_id: String,
    database_id: String,
    token: String,
) -> Result<DatabaseConnection> {
    let proxy = D1Proxy::new(api_base, account_id, database_id, token);
    let db = Database::connect_proxy(DbBackend::Sqlite, Arc::new(Box::new(proxy)))
        .await
        .map_err(LibraryError::Orm)?;
    db.ping().await.map_err(LibraryError::Orm)?;
    crate::migrate::apply_pending_migrations(&db).await?;
    tracing::debug!(plugin = "d1", "opened library database");
    Ok(db)
}

/// SeaORM proxy that executes statements against Cloudflare D1's HTTP API.
#[derive(Debug)]
pub struct D1Proxy {
    api_base: String,
    account_id: String,
    database_id: String,
    api_token: String,
    client: reqwest::Client,
    /// D1 does not support real transactions; begin/commit are nested no-ops
    /// for SeaORM API compatibility. Callers that need atomic multi-statement
    /// updates should batch via D1's HTTP batch API (not yet wired) or accept
    /// best-effort sequencing.
    txn_depth: Mutex<u32>,
}

impl D1Proxy {
    #[must_use]
    pub fn new(
        api_base: String,
        account_id: String,
        database_id: String,
        api_token: String,
    ) -> Self {
        Self {
            api_base: api_base.trim_end_matches('/').to_string(),
            account_id,
            database_id,
            api_token,
            client: reqwest::Client::new(),
            txn_depth: Mutex::new(0),
        }
    }

    fn query_url(&self) -> String {
        format!(
            "{}/accounts/{}/d1/database/{}/query",
            self.api_base, self.account_id, self.database_id
        )
    }

    async fn run_sql(
        &self,
        sql: &str,
        params: Vec<JsonValue>,
    ) -> std::result::Result<JsonValue, DbErr> {
        let body = json!({
            "sql": sql,
            "params": params,
        });
        let response = self
            .client
            .post(self.query_url())
            .bearer_auth(&self.api_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| DbErr::Custom(format!("D1 HTTP error: {e}")))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| DbErr::Custom(format!("D1 read body: {e}")))?;
        if !status.is_success() {
            return Err(DbErr::Custom(format!(
                "D1 HTTP {status}: {}",
                truncate(&text, 500)
            )));
        }
        let value: JsonValue = serde_json::from_str(&text).map_err(|e| {
            DbErr::Custom(format!("D1 JSON parse: {e}; body={}", truncate(&text, 200)))
        })?;
        if value.get("success").and_then(|v| v.as_bool()) != Some(true) {
            return Err(DbErr::Custom(format!(
                "D1 API unsuccessful: {}",
                truncate(&text, 500)
            )));
        }
        Ok(value)
    }
}

#[async_trait]
impl ProxyDatabaseTrait for D1Proxy {
    async fn query(&self, statement: Statement) -> std::result::Result<Vec<ProxyRow>, DbErr> {
        let params = statement_json_params(&statement);
        let value = self.run_sql(&statement.sql, params).await?;
        let results = value
            .get("result")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|first| first.get("results"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut rows = Vec::with_capacity(results.len());
        for row in results {
            let Some(obj) = row.as_object() else {
                continue;
            };
            let mut values = BTreeMap::new();
            for (k, v) in obj {
                values.insert(k.clone(), json_to_sea_value(v, k));
            }
            rows.push(ProxyRow { values });
        }
        Ok(rows)
    }

    async fn execute(&self, statement: Statement) -> std::result::Result<ProxyExecResult, DbErr> {
        let params = statement_json_params(&statement);
        let value = self.run_sql(&statement.sql, params).await?;
        let meta = value
            .get("result")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|first| first.get("meta"));
        let last_insert_id = meta
            .and_then(|m| m.get("last_row_id"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let rows_affected = meta
            .and_then(|m| m.get("changes"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        Ok(ProxyExecResult {
            last_insert_id,
            rows_affected,
        })
    }

    async fn begin(&self) {
        if let Ok(mut depth) = self.txn_depth.lock() {
            *depth = depth.saturating_add(1);
        }
        tracing::debug!("D1 proxy begin (no-op; D1 lacks interactive transactions)");
    }

    async fn commit(&self) {
        if let Ok(mut depth) = self.txn_depth.lock() {
            *depth = depth.saturating_sub(1);
        }
    }

    async fn rollback(&self) {
        if let Ok(mut depth) = self.txn_depth.lock() {
            *depth = depth.saturating_sub(1);
        }
        tracing::warn!("D1 proxy rollback requested (no-op; prior statements already applied)");
    }

    async fn ping(&self) -> std::result::Result<(), DbErr> {
        let _ = self.run_sql("SELECT 1 AS ok;", Vec::new()).await?;
        Ok(())
    }
}

fn statement_json_params(statement: &Statement) -> Vec<JsonValue> {
    match &statement.values {
        Some(values) => values.0.iter().map(sea_value_to_json).collect(),
        None => Vec::new(),
    }
}

fn sea_value_to_json(v: &Value) -> JsonValue {
    match v {
        Value::Bool(Some(b)) => JsonValue::Bool(*b),
        Value::TinyInt(Some(n)) => JsonValue::from(*n),
        Value::SmallInt(Some(n)) => JsonValue::from(*n),
        Value::Int(Some(n)) => JsonValue::from(*n),
        Value::BigInt(Some(n)) => JsonValue::from(*n),
        Value::Float(Some(n)) => JsonValue::from(f64::from(*n)),
        Value::Double(Some(n)) => JsonValue::from(*n),
        Value::String(Some(s)) => JsonValue::String(s.to_string()),
        Value::Bytes(Some(b)) => {
            // D1 does not have a native binary type; encode as b64:… string.
            JsonValue::String(bytes_to_b64_string(b))
        }
        Value::ChronoDateTimeUtc(Some(dt)) => JsonValue::String(dt.to_rfc3339()),
        Value::ChronoDateTime(Some(dt)) => JsonValue::String(dt.and_utc().to_rfc3339()),
        _ => JsonValue::Null,
    }
}

/// Binary column names in `encrypted_secrets` (and other tables) that are
/// always bytes even when not prefixed with `b64:`.
const BINARY_COLUMNS: &[&str] = &[
    "ciphertext",
    "kdf_salt",
    "cipher_nonce",
    "vector", // embeddings BLOB
];

fn is_binary_column(column: &str) -> bool {
    BINARY_COLUMNS.contains(&column)
}

fn json_to_sea_value(v: &JsonValue, column: &str) -> Value {
    match v {
        JsonValue::Null => crate::migrate::typed_null(None, column),
        JsonValue::Bool(b) => Value::Bool(Some(*b)),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::BigInt(Some(i))
            } else if let Some(f) = n.as_f64() {
                Value::Double(Some(f))
            } else {
                Value::String(Some(n.to_string()))
            }
        }
        JsonValue::String(s) => {
            // Decode b64:-prefixed strings or known binary columns.
            if let Some(bytes) = b64_string_to_bytes(s) {
                return Value::Bytes(Some(bytes));
            }
            if is_binary_column(column) {
                // Legacy: try base64 without prefix (shouldn't happen with new writes).
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(s.as_bytes()) {
                    return Value::Bytes(Some(bytes));
                }
            }
            Value::String(Some(s.clone()))
        }
        other => Value::String(Some(other.to_string())),
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}
