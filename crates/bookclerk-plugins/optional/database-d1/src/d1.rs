//! Cloudflare D1 HTTP API proxy for SeaORM.
//!
//! D1 HTTP requests do not keep an interactive SQL transaction open. Each
//! autocommit statement is posted as a one-element D1 batch array. Interactive
//! `BEGIN` is rejected: Time Travel is not a substitute for rollback, and
//! mid-transaction SeaORM reads cannot be satisfied without committing.
//! Atomic library operations use [`D1Proxy::run_atomic`] (`dbAtomic`) which
//! sends one multi-statement HTTP `batch()` (a real SQL transaction) with
//! control flow encoded in SQL.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use base64::Engine as _;
use bookclerk_library::{b64_string_to_bytes, bytes_to_b64_string, LibraryError, Result};
use sea_orm::{
    Database, DatabaseConnection, DbBackend, DbErr, ProxyDatabaseTrait, ProxyExecResult, ProxyRow,
    Statement, Value,
};
use serde_json::{json, Value as JsonValue};
use tokio::sync::Mutex as AsyncMutex;

/// Open Cloudflare D1 with an explicit API token (host-mediated).
pub async fn open(
    api_base: String,
    account_id: String,
    database_id: String,
    token: String,
) -> Result<DatabaseConnection> {
    let proxy = D1Proxy::new(api_base, account_id, database_id, token);
    set_shared_proxy(proxy.clone());
    let db = Database::connect_proxy(DbBackend::Sqlite, Arc::new(Box::new(proxy)))
        .await
        .map_err(LibraryError::Orm)?;
    db.ping().await.map_err(LibraryError::Orm)?;
    bookclerk_db_guest::apply_pending_migrations(&db).await?;
    tracing::debug!(plugin = "d1", "opened library database");
    Ok(db)
}

const D1_INTERACTIVE_TXN_UNSUPPORTED: &str = "D1 does not support interactive transactions; \
     each HTTP request commits immediately. Use dbAtomic for claim redeem and last-owner guards";

static SHARED: OnceLock<D1Proxy> = OnceLock::new();

/// Stores the process-wide D1 proxy used by `dbAtomic` after [`open`].
pub fn set_shared_proxy(proxy: D1Proxy) {
    let _ = SHARED.set(proxy);
}

/// Process-wide D1 proxy installed by [`open`], if any.
#[must_use]
pub fn shared_proxy() -> Option<D1Proxy> {
    SHARED.get().cloned()
}

struct D1Inner {
    api_base: String,
    account_id: String,
    database_id: String,
    api_token: String,
    client: reqwest::Client,
    /// Serializes HTTP requests to a single D1 database.
    http: AsyncMutex<()>,
}

/// SeaORM proxy that executes statements against Cloudflare D1's HTTP API.
#[derive(Clone)]
pub struct D1Proxy {
    inner: Arc<D1Inner>,
}

impl std::fmt::Debug for D1Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("D1Proxy")
            .field("account_id", &self.inner.account_id)
            .field("database_id", &self.inner.database_id)
            .finish_non_exhaustive()
    }
}

impl D1Proxy {
    /// Constructs a new instance with default or provided parameters.
    #[must_use]
    pub fn new(
        api_base: String,
        account_id: String,
        database_id: String,
        api_token: String,
    ) -> Self {
        Self {
            inner: Arc::new(D1Inner {
                api_base: api_base.trim_end_matches('/').to_string(),
                account_id,
                database_id,
                api_token,
                client: reqwest::Client::new(),
                http: AsyncMutex::new(()),
            }),
        }
    }

    fn query_url(&self) -> String {
        format!(
            "{}/accounts/{}/d1/database/{}/query",
            self.inner.api_base, self.inner.account_id, self.inner.database_id
        )
    }

    async fn post_json(&self, url: &str, body: JsonValue) -> std::result::Result<JsonValue, DbErr> {
        let _http = self.inner.http.lock().await;
        let response = self
            .inner
            .client
            .post(url)
            .bearer_auth(&self.inner.api_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| DbErr::Custom(format!("D1 HTTP error: {e}")))?;
        parse_d1_response(response).await
    }

    /// Runs one or more SQL statements. A JSON array is a D1 batch: statements
    /// execute sequentially as one SQL transaction and roll back together on
    /// failure.
    pub(crate) async fn run_batch(
        &self,
        statements: &[(String, Vec<JsonValue>)],
    ) -> std::result::Result<JsonValue, DbErr> {
        if statements.is_empty() {
            return Err(DbErr::Custom(
                "D1 batch requires at least one statement".into(),
            ));
        }
        let body = JsonValue::Array(
            statements
                .iter()
                .map(|(sql, params)| {
                    json!({
                        "sql": sql,
                        "params": params,
                    })
                })
                .collect(),
        );
        self.post_json(&self.query_url(), body).await
    }

    async fn run_sql(
        &self,
        sql: &str,
        params: Vec<JsonValue>,
    ) -> std::result::Result<JsonValue, DbErr> {
        self.run_batch(&[(sql.to_string(), params)]).await
    }
}

#[async_trait]
impl ProxyDatabaseTrait for D1Proxy {
    async fn query(&self, statement: Statement) -> std::result::Result<Vec<ProxyRow>, DbErr> {
        if bookclerk_library::is_txn_broken() {
            return Err(bookclerk_library::txn_broken_err());
        }
        let params = statement_json_params(&statement);
        let value = self.run_sql(&statement.sql, params).await?;
        let results = first_result_rows(&value);
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
        if bookclerk_library::is_txn_broken() {
            return Err(bookclerk_library::txn_broken_err());
        }
        let params = statement_json_params(&statement);
        let value = self.run_sql(&statement.sql, params).await?;
        let meta = first_result_meta(&value);
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
        bookclerk_library::note_begin_failed(D1_INTERACTIVE_TXN_UNSUPPORTED);
    }

    async fn commit(&self) {}

    async fn rollback(&self) {}

    fn start_rollback(&self) {}

    async fn ping(&self) -> std::result::Result<(), DbErr> {
        if bookclerk_library::is_txn_broken() {
            return Err(bookclerk_library::txn_broken_err());
        }
        let _ = self.run_sql("SELECT 1 AS ok;", Vec::new()).await?;
        Ok(())
    }
}

async fn parse_d1_response(response: reqwest::Response) -> std::result::Result<JsonValue, DbErr> {
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
    let value: JsonValue = serde_json::from_str(&text)
        .map_err(|e| DbErr::Custom(format!("D1 JSON parse: {e}; body={}", truncate(&text, 200))))?;
    if value.get("success").and_then(|v| v.as_bool()) == Some(false) {
        return Err(DbErr::Custom(format!(
            "D1 API unsuccessful: {}",
            truncate(&text, 500)
        )));
    }
    Ok(value)
}

fn first_result_entry(value: &JsonValue) -> Option<&JsonValue> {
    value
        .get("result")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.last())
}

fn first_result_rows(value: &JsonValue) -> Vec<JsonValue> {
    first_result_entry(value)
        .and_then(|first| first.get("results"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

fn first_result_meta(value: &JsonValue) -> Option<&JsonValue> {
    first_result_entry(value).and_then(|first| first.get("meta"))
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
        JsonValue::Null => bookclerk_db_guest::migrate::typed_null(None, column),
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

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{DatabaseBackend, ProxyDatabaseTrait, Statement};
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn query_ok() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "result": [{
                "results": [],
                "success": true,
                "meta": { "changes": 1, "last_row_id": 1 }
            }]
        }))
    }

    #[tokio::test]
    async fn begin_rejects_interactive_transactions() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/query$"))
            .respond_with(query_ok())
            .mount(&server)
            .await;

        let proxy = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        proxy.begin().await;
        let stmt =
            Statement::from_string(DatabaseBackend::Sqlite, "INSERT INTO t (v) VALUES ('x')");
        let err = proxy.execute(stmt).await.unwrap_err();
        assert!(
            err.to_string().contains("interactive transactions"),
            "{err}"
        );
        let _ = bookclerk_library::take_txn_fault();
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn atomic_redeem_posts_one_multi_statement_batch() {
        use bookclerk_plugin_sdk::DbAtomicParams;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/query$"))
            .respond_with(query_ok())
            .mount(&server)
            .await;

        let proxy = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        let _ = proxy
            .run_atomic(DbAtomicParams::RedeemClaimTicket {
                token_hash: "ticket".into(),
                session_hash: "session".into(),
                expires_at: "2099-01-01T00:00:00Z".into(),
                user_agent: None,
                device_type: None,
                client_label: None,
                new_password_hash: Some("hash".into()),
            })
            .await;

        let queries: Vec<JsonValue> = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path().ends_with("/query"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        assert_eq!(queries.len(), 1);
        assert!(queries[0].is_array());
        assert!(
            queries[0].as_array().unwrap().len() > 1,
            "dbAtomic must send a multi-statement D1 batch, got {}",
            queries[0]
        );
        let sql = queries[0][0]["sql"].as_str().unwrap();
        assert!(sql.contains("claim_tickets"), "{sql}");
    }

    #[tokio::test]
    async fn statements_are_posted_as_d1_batch_arrays() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/query$"))
            .respond_with(query_ok())
            .mount(&server)
            .await;

        let proxy = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        let stmt = Statement::from_string(DatabaseBackend::Sqlite, "SELECT 1");
        proxy.query(stmt).await.unwrap();

        let queries: Vec<JsonValue> = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path().ends_with("/query"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        assert_eq!(queries.len(), 1);
        assert!(queries[0].is_array(), "D1 batch body must be a JSON array");
        assert_eq!(queries[0][0]["sql"], "SELECT 1");
    }
}
