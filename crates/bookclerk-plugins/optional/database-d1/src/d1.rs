//! Cloudflare D1 HTTP API proxy for SeaORM.
//!
//! D1 HTTP requests do not keep an interactive SQL transaction open.
//! Autocommit statements use the documented `{ "sql", "params" }` REST body.
//! Interactive `BEGIN` is rejected: Time Travel is not a substitute for rollback, and
//! mid-transaction SeaORM reads cannot be satisfied without committing.
//! Atomic library operations use [`D1Proxy::run_atomic`] (`dbAtomic`) which
//! sends `{ "batch": [...] }` (a real SQL transaction) with control flow
//! encoded in SQL and a durable `operationId` receipt.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use bookclerk_library::{b64_string_to_bytes, bytes_to_b64_string, LibraryError, Result};
use bookclerk_plugin_sdk::v2::MAX_SCALAR_BYTES;
use sea_orm::{
    Database, DatabaseConnection, DbBackend, DbErr, ProxyDatabaseTrait, ProxyExecResult, ProxyRow,
    Statement, Value,
};
use serde_json::{json, Value as JsonValue};
use tokio::sync::Mutex as AsyncMutex;

/// Open Cloudflare D1 with an explicit API token (host-mediated).
///
/// # Errors
///
/// Returns an error when the operation fails.
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

/// Operator-facing reason recorded when SeaORM `begin` is rejected on D1 HTTP.
const D1_INTERACTIVE_TXN_UNSUPPORTED: &str = "D1 does not support interactive transactions; \
     each HTTP request commits immediately. Use dbAtomic for claim redeem, last-owner guards, and consume-once tokens";

/// Process-wide D1 proxy installed by [`open`] so `dbAtomic` can reuse the same HTTP client.
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

/// Cloudflare account/database ids plus the HTTP client used by [`D1Proxy`].
struct D1Inner {
    /// Cloudflare API origin with no trailing slash (e.g. `https://api.cloudflare.com/client/v4`).
    api_base: String,
    /// Cloudflare account id that owns the D1 database.
    account_id: String,
    /// D1 database UUID used in `/d1/database/{id}/query`.
    database_id: String,
    /// Cloudflare API token injected by the host; never logged.
    api_token: String,
    /// Shared HTTP client with connect/request timeouts below the host RPC deadline.
    client: reqwest::Client,
    /// Serializes HTTP requests to a single D1 database.
    http: AsyncMutex<()>,
}

/// SeaORM proxy that executes statements against Cloudflare D1's HTTP API.
#[derive(Clone)]
pub struct D1Proxy {
    /// Shared connection state; [`D1Proxy`] is cheaply cloneable.
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

/// HTTP budget for one D1 request, well below the host plugin RPC deadline (300s).
const D1_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// TCP connect budget for one D1 request (10s), well below the 20s request timeout.
const D1_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Classified D1 HTTP / transport failure.
#[derive(Debug)]
pub(crate) enum D1Error {
    /// Transport, timeout, incomplete/garbled 2xx, or retryable HTTP (408/429/5xx).
    Ambiguous {
        /// Operator-facing transport / timeout / 5xx text (truncated upstream).
        message: String,
        /// Optional `Retry-After` delay parsed from the Cloudflare response, in seconds.
        retry_after: Option<Duration>,
    },
    /// Permanent HTTP failure (typical 4xx). Do not retry.
    Permanent {
        /// HTTP status returned by D1 / Cloudflare.
        status: u16,
        /// Operator-facing error text from the upstream response.
        message: String,
    },
}

impl D1Error {
    /// Classifies a transport or incomplete-2xx failure as retryable with no Retry-After.
    fn ambiguous(message: impl Into<String>) -> Self {
        Self::Ambiguous {
            message: message.into(),
            retry_after: None,
        }
    }

    /// True for ambiguous transport / 408 / 429 / 5xx failures that the guest may retry.
    pub(crate) fn is_retryable(&self) -> bool {
        matches!(self, Self::Ambiguous { .. })
    }

    /// Suggested backoff from `Retry-After` on ambiguous errors; `None` for permanent failures.
    pub(crate) fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Ambiguous { retry_after, .. } => *retry_after,
            Self::Permanent { .. } => None,
        }
    }
}

impl From<D1Error> for DbErr {
    fn from(err: D1Error) -> Self {
        match err {
            D1Error::Ambiguous {
                message,
                retry_after,
            } => {
                let extra = retry_after
                    .map(|d| format!(" retry-after-ms={}", d.as_millis()))
                    .unwrap_or_default();
                DbErr::Custom(format!("D1 ambiguous response:{extra} {message}"))
            }
            D1Error::Permanent { status, message } => {
                DbErr::Custom(format!("D1 HTTP {status}: {message}"))
            }
        }
    }
}

/// True for HTTP 408, 429, or any 5xx (safe to retry the same D1 statement).
fn retryable_http_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

/// Parses a numeric `Retry-After` header as seconds; HTTP-date values are ignored.
fn parse_retry_after(response: &reqwest::Response) -> Option<Duration> {
    let raw = response.headers().get(reqwest::header::RETRY_AFTER)?;
    let s = raw.to_str().ok()?.trim();
    s.parse::<u64>().ok().map(Duration::from_secs)
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
        let client = reqwest::Client::builder()
            .timeout(D1_REQUEST_TIMEOUT)
            .connect_timeout(D1_CONNECT_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            inner: Arc::new(D1Inner {
                api_base: api_base.trim_end_matches('/').to_string(),
                account_id,
                database_id,
                api_token,
                client,
                http: AsyncMutex::new(()),
            }),
        }
    }

    /// Cloudflare D1 `/query` URL for this account and database.
    fn query_url(&self) -> String {
        format!(
            "{}/accounts/{}/d1/database/{}/query",
            self.inner.api_base, self.inner.account_id, self.inner.database_id
        )
    }

    /// POSTs a JSON body with the API token; serializes concurrent requests to one database.
    ///
    /// Every D1 JSON body is read incrementally and aborted at
    /// [`max_d1_http_body_bytes`] (page scalar budget plus a narrow envelope).
    async fn post_json(
        &self,
        url: &str,
        body: JsonValue,
    ) -> std::result::Result<JsonValue, D1Error> {
        let _http = self.inner.http.lock().await;
        let response = self
            .inner
            .client
            .post(url)
            .bearer_auth(&self.inner.api_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| D1Error::ambiguous(format!("transport: {e}")))?;
        parse_d1_response(response).await
    }

    /// Runs one or more SQL statements as one documented D1 `{ "batch": [...] }`
    /// request: statements execute sequentially as one SQL transaction and roll
    /// back together on failure.
    pub(crate) async fn run_batch(
        &self,
        statements: &[(String, Vec<JsonValue>)],
    ) -> std::result::Result<JsonValue, D1Error> {
        if statements.is_empty() {
            return Err(D1Error::Permanent {
                status: 400,
                message: "D1 batch requires at least one statement".into(),
            });
        }
        let body = json!({
            "batch": statements
                .iter()
                .map(|(sql, params)| json!({ "sql": sql, "params": params }))
                .collect::<Vec<_>>(),
        });
        self.post_json(&self.query_url(), body).await
    }

    /// Runs one autocommit `{ sql, params }` statement (not an interactive transaction).
    async fn run_sql(
        &self,
        sql: &str,
        params: Vec<JsonValue>,
    ) -> std::result::Result<JsonValue, DbErr> {
        let body = json!({ "sql": sql, "params": params });
        self.post_json(&self.query_url(), body)
            .await
            .map_err(DbErr::from)
    }
}

#[async_trait]
impl ProxyDatabaseTrait for D1Proxy {
    async fn query(&self, statement: Statement) -> std::result::Result<Vec<ProxyRow>, DbErr> {
        if bookclerk_library::is_txn_broken() {
            return Err(bookclerk_library::txn_broken_err());
        }
        let params = statement_json_params(&statement);
        let paged = sql_is_bookclerk_page(&statement.sql);
        let value = self.run_sql(&statement.sql, params).await?;
        let results = first_result_rows_ref(&value);
        let mut rows = Vec::with_capacity(results.len());
        let mut encoded = 1usize;
        for row in results {
            let Some(obj) = row.as_object() else {
                continue;
            };
            let mut values = BTreeMap::new();
            for (k, v) in obj {
                reject_oversized_json_cell(v, k)?;
                if paged {
                    let extra = json_cell_byte_len(v).saturating_add(k.len());
                    if encoded.saturating_add(extra) > MAX_SCALAR_BYTES as usize {
                        return Err(DbErr::Custom(format!(
                            "JSON result would exceed {MAX_SCALAR_BYTES}"
                        )));
                    }
                    encoded = encoded.saturating_add(extra);
                }
                values.insert(k.clone(), json_to_sea_value(v, k)?);
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

/// Reads a D1 HTTP response; retryable statuses become [`D1Error::Ambiguous`], others permanent.
///
/// The body is pulled in chunks and aborted at byte `max + 1` so a chunked or
/// lying-small `Content-Length` cannot buffer past the page scalar budget plus
/// a narrow D1 JSON envelope.
async fn parse_d1_response(response: reqwest::Response) -> std::result::Result<JsonValue, D1Error> {
    let status = response.status();
    let retry_after = parse_retry_after(&response);
    let max = max_d1_http_body_bytes();
    let bytes = read_body_capped(response, max).await?;
    let text = String::from_utf8_lossy(&bytes);
    if !status.is_success() {
        let message = truncate(text.as_ref(), 500).to_string();
        return Err(if retryable_http_status(status) {
            D1Error::Ambiguous {
                message: format!("HTTP {status}: {message}"),
                retry_after,
            }
        } else {
            D1Error::Permanent {
                status: status.as_u16(),
                message,
            }
        });
    }
    let value: JsonValue = serde_json::from_slice(&bytes).map_err(|e| {
        D1Error::ambiguous(format!(
            "JSON parse: {e}; body={}",
            truncate(text.as_ref(), 200)
        ))
    })?;
    if value.get("success").and_then(|v| v.as_bool()) == Some(false) {
        return Err(D1Error::Permanent {
            status: status.as_u16(),
            message: format!("API unsuccessful: {}", truncate(text.as_ref(), 500)),
        });
    }
    Ok(value)
}

/// Incrementally reads `response` and errors if the body would exceed `max`.
async fn read_body_capped(
    mut response: reqwest::Response,
    max: usize,
) -> std::result::Result<Vec<u8>, D1Error> {
    if let Some(len) = response.content_length() {
        if len > max as u64 {
            return Err(D1Error::Permanent {
                status: response.status().as_u16(),
                message: format!("D1 body {len} bytes exceeds {max}"),
            });
        }
    }
    let mut buf = Vec::new();
    loop {
        let chunk = response
            .chunk()
            .await
            .map_err(|e| D1Error::ambiguous(format!("read body: {e}")))?;
        let Some(chunk) = chunk else {
            break;
        };
        if buf.len().saturating_add(chunk.len()) > max {
            return Err(D1Error::Permanent {
                status: response.status().as_u16(),
                message: format!("D1 body exceeds {max}"),
            });
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Last entry in the D1 `result` array (batch responses keep per-statement results).
fn first_result_entry(value: &JsonValue) -> Option<&JsonValue> {
    value
        .get("result")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.last())
}

/// Row objects from the last result entry's `results` array, or empty.
fn first_result_rows_ref(value: &JsonValue) -> &[JsonValue] {
    first_result_entry(value)
        .and_then(|first| first.get("results"))
        .and_then(|v| v.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// `meta` object (`changes`, `last_row_id`) from the last result entry.
fn first_result_meta(value: &JsonValue) -> Option<&JsonValue> {
    first_result_entry(value).and_then(|first| first.get("meta"))
}

/// Converts SeaORM bound values to the JSON array D1's REST body expects.
fn statement_json_params(statement: &Statement) -> Vec<JsonValue> {
    match &statement.values {
        Some(values) => values.0.iter().map(sea_value_to_json).collect(),
        None => Vec::new(),
    }
}

/// Maps a SeaORM [`Value`] to JSON; BLOBs become `b64:…` strings because D1 has no binary type.
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

/// True for `encrypted_secrets` / embeddings columns that must decode as bytes even without a `b64:` prefix.
fn is_binary_column(column: &str) -> bool {
    BINARY_COLUMNS.contains(&column)
}

/// Maps a D1 JSON cell back to a SeaORM [`Value`], decoding `b64:` or known binary columns.
///
/// String and blob cells larger than [`MAX_SCALAR_BYTES`] are rejected before
/// clone or base64 decode.
fn json_to_sea_value(v: &JsonValue, column: &str) -> std::result::Result<Value, DbErr> {
    reject_oversized_json_cell(v, column)?;
    match v {
        JsonValue::Null => Ok(bookclerk_db_guest::migrate::typed_null(None, column)),
        JsonValue::Bool(b) => Ok(Value::Bool(Some(*b))),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::BigInt(Some(i)))
            } else if let Some(f) = n.as_f64() {
                Ok(Value::Double(Some(f)))
            } else {
                Ok(Value::String(Some(n.to_string())))
            }
        }
        JsonValue::String(s) => {
            // Decode b64:-prefixed strings or known binary columns.
            if let Some(bytes) = b64_string_to_bytes(s) {
                if bytes.len() > MAX_SCALAR_BYTES as usize {
                    return Err(oversized_cell_err(column, bytes.len()));
                }
                return Ok(Value::Bytes(Some(bytes)));
            }
            if is_binary_column(column) {
                // Legacy: try base64 without prefix (shouldn't happen with new writes).
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(s.as_bytes()) {
                    if bytes.len() > MAX_SCALAR_BYTES as usize {
                        return Err(oversized_cell_err(column, bytes.len()));
                    }
                    return Ok(Value::Bytes(Some(bytes)));
                }
            }
            Ok(Value::String(Some(s.clone())))
        }
        other => {
            let encoded = other.to_string();
            if encoded.len() > MAX_SCALAR_BYTES as usize {
                return Err(oversized_cell_err(column, encoded.len()));
            }
            Ok(Value::String(Some(encoded)))
        }
    }
}

/// True when SQL is the guest page wrapper (`AS _bookclerk_page LIMIT …`).
fn sql_is_bookclerk_page(sql: &str) -> bool {
    sql.contains("_bookclerk_page")
}

/// Cloudflare JSON envelope around row payload (`success` / `result` / `meta`).
const D1_JSON_ENVELOPE_BYTES: usize = 4096;

/// HTTP-body ceiling for every D1 JSON response: one page of scalars plus envelope.
fn max_d1_http_body_bytes() -> usize {
    (MAX_SCALAR_BYTES as usize).saturating_add(D1_JSON_ENVELOPE_BYTES)
}

/// UTF-8 / JSON length of one D1 cell, used to accumulate a page budget.
fn json_cell_byte_len(v: &JsonValue) -> usize {
    match v {
        JsonValue::String(s) => s.len(),
        JsonValue::Array(_) | JsonValue::Object(_) => v.to_string().len(),
        _ => 0,
    }
}

/// Rejects a string or blob JSON cell before it is cloned into a SeaORM value.
fn reject_oversized_json_cell(v: &JsonValue, column: &str) -> std::result::Result<(), DbErr> {
    let nbytes = json_cell_byte_len(v);
    if nbytes > MAX_SCALAR_BYTES as usize {
        return Err(oversized_cell_err(column, nbytes));
    }
    Ok(())
}

/// Operator-facing error when one decoded cell exceeds [`MAX_SCALAR_BYTES`].
fn oversized_cell_err(column: &str, nbytes: usize) -> DbErr {
    DbErr::Custom(format!(
        "column `{column}` is {nbytes} bytes; exceeds {MAX_SCALAR_BYTES}"
    ))
}

/// Truncates an error body to `max` bytes so logs stay bounded.
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

    fn incomplete_batch_ok() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "result": []
        }))
    }

    fn d1_success_for_batch_request(request: &wiremock::Request) -> ResponseTemplate {
        let body: JsonValue = serde_json::from_slice(&request.body).unwrap_or(json!({}));
        let batch = body
            .get("batch")
            .and_then(JsonValue::as_array)
            .cloned()
            .unwrap_or_else(|| vec![json!({"sql": "SELECT 1"})]);
        let (echo_op, echo_hash) = receipt_echo_from_batch(&batch);
        let results: Vec<JsonValue> = batch
            .iter()
            .map(|stmt| {
                let sql = stmt.get("sql").and_then(JsonValue::as_str).unwrap_or("");
                let is_receipt_select = sql.contains("FROM db_atomic_receipts")
                    && sql.trim_start().starts_with("SELECT");
                if is_receipt_select {
                    json!({
                        "results": [{
                            "operation_id": echo_op,
                            "request_hash": echo_hash,
                            "status": "empty",
                            "payload": null,
                            "created_at": "2020-01-01T00:00:00Z"
                        }],
                        "success": true,
                        "meta": {}
                    })
                } else {
                    json!({
                        "results": [],
                        "success": true,
                        "meta": { "changes": 1, "last_row_id": 1 }
                    })
                }
            })
            .collect();
        ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "result": results
        }))
    }

    fn receipt_echo_from_batch(batch: &[JsonValue]) -> (String, String) {
        for stmt in batch {
            let sql = stmt.get("sql").and_then(JsonValue::as_str).unwrap_or("");
            if sql.contains("INTO db_atomic_receipts") {
                let params = stmt.get("params").and_then(JsonValue::as_array);
                if let Some(params) = params {
                    let op = params
                        .first()
                        .and_then(JsonValue::as_str)
                        .unwrap_or("op")
                        .to_string();
                    let hash = params
                        .get(2)
                        .and_then(JsonValue::as_str)
                        .filter(|s| !s.is_empty())
                        .unwrap_or("echo-hash")
                        .to_string();
                    return (op, hash);
                }
            }
        }
        ("op".into(), "echo-hash".into())
    }

    struct EchoBatchOk;
    impl wiremock::Respond for EchoBatchOk {
        fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
            d1_success_for_batch_request(request)
        }
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
        use bookclerk_plugin_sdk::{DbAtomicParams, DbAtomicRequest};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/query$"))
            .respond_with(EchoBatchOk)
            .mount(&server)
            .await;

        let proxy = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        let _ = proxy
            .run_atomic(DbAtomicRequest {
                operation_id: "op-redeem".into(),
                operation: DbAtomicParams::RedeemClaimTicket {
                    token_hash: "ticket".into(),
                    session_hash: "session".into(),
                    expires_at: "2099-01-01T00:00:00Z".into(),
                    user_agent: None,
                    device_type: None,
                    client_label: None,
                    new_password_hash: Some("hash".into()),
                    password_fingerprint: Some("fp".into()),
                },
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
        let batch = queries[0]["batch"]
            .as_array()
            .expect("dbAtomic must use the documented { batch: [...] } envelope");
        assert!(
            batch.len() > 1,
            "dbAtomic must send a multi-statement D1 batch, got {}",
            queries[0]
        );
        let sql: String = batch
            .iter()
            .filter_map(|stmt| stmt["sql"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(sql.contains("claim_tickets"), "{sql}");
        assert!(sql.contains("db_atomic_receipts"), "{sql}");
    }

    #[tokio::test]
    async fn atomic_confirm_totp_posts_one_multi_statement_batch() {
        use bookclerk_plugin_sdk::{DbAtomicParams, DbAtomicRequest};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/query$"))
            .respond_with(EchoBatchOk)
            .mount(&server)
            .await;

        let proxy = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        let _ = proxy
            .run_atomic(DbAtomicRequest {
                operation_id: "op-totp".into(),
                operation: DbAtomicParams::ConfirmTotpEnrollment {
                    user_id: 7,
                    format: "sealed-v1".into(),
                    ciphertext: "b64:AA==".into(),
                    cipher_algorithm: Some("xchacha20poly1305".into()),
                    cipher_nonce: Some("b64:AA==".into()),
                    kdf_algorithm: None,
                    kdf_salt: None,
                    kdf_m_cost: None,
                    kdf_t_cost: None,
                    kdf_p_cost: None,
                    created_at: "2024-06-01T00:00:00Z".into(),
                },
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
        let batch = queries[0]["batch"]
            .as_array()
            .expect("dbAtomic must use the documented { batch: [...] } envelope");
        assert!(
            batch.len() > 1,
            "dbAtomic must send a multi-statement D1 batch, got {}",
            queries[0]
        );
        let sql: String = batch
            .iter()
            .filter_map(|stmt| stmt["sql"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(sql.contains("encrypted_secrets"), "{sql}");
        assert!(sql.contains("totp_enabled"), "{sql}");
        assert!(sql.contains("db_atomic_receipts"), "{sql}");
    }

    #[tokio::test]
    async fn atomic_take_oidc_posts_delete_returning_batch() {
        use bookclerk_plugin_sdk::{DbAtomicParams, DbAtomicRequest};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/query$"))
            .respond_with(EchoBatchOk)
            .mount(&server)
            .await;

        let proxy = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        let _ = proxy
            .run_atomic(DbAtomicRequest {
                operation_id: "op-take".into(),
                operation: DbAtomicParams::TakeOidcRpState {
                    state_hash: "abc".into(),
                },
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
        let batch = queries[0]["batch"]
            .as_array()
            .expect("dbAtomic must use the documented { batch: [...] } envelope");
        let sql: String = batch
            .iter()
            .filter_map(|stmt| stmt["sql"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(sql.contains("DELETE FROM oidc_rp_states"), "{sql}");
        assert!(sql.contains("consume_key"), "{sql}");
        assert!(sql.contains("db_atomic_receipts"), "{sql}");
    }

    #[tokio::test]
    async fn atomic_take_oidc_retries_mangled_response() {
        use bookclerk_plugin_sdk::{DbAtomicParams, DbAtomicRequest};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use wiremock::Respond;

        struct FirstMangledThenOk {
            hits: Arc<AtomicUsize>,
        }
        impl Respond for FirstMangledThenOk {
            fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
                if self.hits.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(200).set_body_string("{\"success\":")
                } else {
                    d1_success_for_batch_request(request)
                }
            }
        }

        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path_regex(r"/query$"))
            .respond_with(FirstMangledThenOk { hits: hits.clone() })
            .mount(&server)
            .await;

        let proxy = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        let result = proxy
            .run_atomic(DbAtomicRequest {
                operation_id: "op-retry".into(),
                operation: DbAtomicParams::TakeOidcRpState {
                    state_hash: "abc".into(),
                },
            })
            .await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn atomic_take_oidc_retries_incomplete_2xx() {
        use bookclerk_plugin_sdk::{DbAtomicParams, DbAtomicRequest};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use wiremock::Respond;

        struct FirstIncompleteThenOk {
            hits: Arc<AtomicUsize>,
        }
        impl Respond for FirstIncompleteThenOk {
            fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
                if self.hits.fetch_add(1, Ordering::SeqCst) == 0 {
                    incomplete_batch_ok()
                } else {
                    d1_success_for_batch_request(request)
                }
            }
        }

        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path_regex(r"/query$"))
            .respond_with(FirstIncompleteThenOk { hits: hits.clone() })
            .mount(&server)
            .await;

        let proxy = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        let result = proxy
            .run_atomic(DbAtomicRequest {
                operation_id: "op-incomplete".into(),
                operation: DbAtomicParams::TakeOidcRpState {
                    state_hash: "abc".into(),
                },
            })
            .await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn atomic_take_oidc_outer_retry_reuses_operation_id_after_two_lost_replies() {
        use bookclerk_plugin_sdk::{DbAtomicParams, DbAtomicRequest};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use wiremock::Respond;

        struct TwoIncompleteThenOk {
            hits: Arc<AtomicUsize>,
        }
        impl Respond for TwoIncompleteThenOk {
            fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
                if self.hits.fetch_add(1, Ordering::SeqCst) < 2 {
                    incomplete_batch_ok()
                } else {
                    d1_success_for_batch_request(request)
                }
            }
        }

        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path_regex(r"/query$"))
            .respond_with(TwoIncompleteThenOk { hits: hits.clone() })
            .mount(&server)
            .await;

        let proxy = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        let req = DbAtomicRequest {
            operation_id: "op-outer".into(),
            operation: DbAtomicParams::TakeOidcRpState {
                state_hash: "abc".into(),
            },
        };
        // Inner loop retries twice on incomplete 2xx, then succeeds on the third
        // attempt with the same operation_id (the outer caller also reuses it).
        let result = proxy.run_atomic(req.clone()).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 3);

        let replay = proxy.run_atomic(req).await.unwrap();
        assert_eq!(replay.operation_id, "op-outer");
        assert!(hits.load(Ordering::SeqCst) >= 4);
    }

    #[tokio::test]
    async fn atomic_take_oidc_exhausted_inner_retries_then_same_id_recovers() {
        use bookclerk_plugin_sdk::{DbAtomicParams, DbAtomicRequest};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use wiremock::Respond;

        struct ThreeIncompleteThenOk {
            hits: Arc<AtomicUsize>,
        }
        impl Respond for ThreeIncompleteThenOk {
            fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
                if self.hits.fetch_add(1, Ordering::SeqCst) < 3 {
                    incomplete_batch_ok()
                } else {
                    d1_success_for_batch_request(request)
                }
            }
        }

        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path_regex(r"/query$"))
            .respond_with(ThreeIncompleteThenOk { hits: hits.clone() })
            .mount(&server)
            .await;

        let proxy = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        let req = DbAtomicRequest {
            operation_id: "op-resume".into(),
            operation: DbAtomicParams::TakeOidcRpState {
                state_hash: "abc".into(),
            },
        };
        let first = proxy.run_atomic(req.clone()).await;
        assert!(first.is_err(), "three incomplete 2xx exhaust inner retries");
        assert_eq!(hits.load(Ordering::SeqCst), 3);

        let recovered = proxy.run_atomic(req).await;
        assert!(recovered.is_ok(), "{recovered:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn atomic_permanent_400_is_not_retried() {
        use bookclerk_plugin_sdk::{DbAtomicParams, DbAtomicRequest};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use wiremock::Respond;

        struct Always400 {
            hits: Arc<AtomicUsize>,
        }
        impl Respond for Always400 {
            fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
                self.hits.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(400)
                    .set_body_string("{\"success\":false,\"errors\":[\"bad\"]}")
            }
        }

        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path_regex(r"/query$"))
            .respond_with(Always400 { hits: hits.clone() })
            .mount(&server)
            .await;

        let proxy = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        let err = proxy
            .run_atomic(DbAtomicRequest {
                operation_id: "op-400".into(),
                operation: DbAtomicParams::TakeOidcRpState {
                    state_hash: "abc".into(),
                },
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("D1 HTTP 400"), "{err}");
        assert!(!crate::atomic::is_ambiguous_d1(&err), "{err}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn atomic_503_is_retried_then_succeeds() {
        use bookclerk_plugin_sdk::{DbAtomicParams, DbAtomicRequest};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use wiremock::Respond;

        struct First503ThenOk {
            hits: Arc<AtomicUsize>,
        }
        impl Respond for First503ThenOk {
            fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
                if self.hits.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(503)
                        .insert_header("Retry-After", "0")
                        .set_body_string("unavailable")
                } else {
                    d1_success_for_batch_request(request)
                }
            }
        }

        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path_regex(r"/query$"))
            .respond_with(First503ThenOk { hits: hits.clone() })
            .mount(&server)
            .await;

        let proxy = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        let result = proxy
            .run_atomic(DbAtomicRequest {
                operation_id: "op-503".into(),
                operation: DbAtomicParams::TakeOidcRpState {
                    state_hash: "abc".into(),
                },
            })
            .await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
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
        assert!(
            queries[0].is_object(),
            "single-statement D1 query must use {{ sql, params }}"
        );
        assert_eq!(queries[0]["sql"], "SELECT 1");
    }

    fn page_rows_ok(n: usize) -> ResponseTemplate {
        let results: Vec<JsonValue> = (0..n).map(|i| json!({ "id": i })).collect();
        ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "result": [{
                "results": results,
                "success": true,
                "meta": {}
            }]
        }))
    }

    #[test]
    fn sql_is_bookclerk_page_detects_alias() {
        assert!(sql_is_bookclerk_page(
            "SELECT * FROM (SELECT id FROM t) AS _bookclerk_page LIMIT 11 OFFSET 0"
        ));
        assert!(!sql_is_bookclerk_page("SELECT 1"));
    }

    #[test]
    fn json_to_sea_value_rejects_oversized_string_before_clone() {
        let huge = "x".repeat(MAX_SCALAR_BYTES as usize + 1);
        let err = json_to_sea_value(&JsonValue::String(huge), "v").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(&MAX_SCALAR_BYTES.to_string()) || msg.contains("exceeds"),
            "expected scalar-cap error, got {msg}"
        );
    }

    #[tokio::test]
    async fn paged_query_uses_one_http_request_not_per_row() {
        use std::sync::Arc;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/query$"))
            .respond_with(page_rows_ok(11))
            .mount(&server)
            .await;

        let proxy = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        let db = Database::connect_proxy(DbBackend::Sqlite, Arc::new(Box::new(proxy)))
            .await
            .unwrap();
        bookclerk_db_guest::set_connection(db).await;
        let page = bookclerk_db_guest::guest_query_page(
            bookclerk_plugin_sdk::StatementDto {
                sql: "SELECT id FROM t".into(),
                values: Vec::new(),
                txn_id: None,
            },
            "",
            10,
        )
        .await
        .unwrap();
        let rows: Vec<JsonValue> = serde_json::from_str(&page.rows_json).unwrap();
        assert_eq!(rows.len(), 10);
        assert_eq!(page.next_cursor.as_deref(), Some("10"));
        let requests = server.received_requests().await.unwrap();
        let hits = requests
            .iter()
            .filter(|r| r.url.path().ends_with("/query"))
            .count();
        assert_eq!(
            hits, 1,
            "paged D1 query must be one LIMIT+1 HTTP request, got {hits}"
        );
        let body: JsonValue = serde_json::from_slice(&requests[0].body).unwrap();
        let sql = body["sql"].as_str().unwrap();
        assert!(sql.contains("_bookclerk_page"), "{sql}");
        assert!(sql.contains("LIMIT 11"), "{sql}");
    }

    #[tokio::test]
    async fn paged_query_rejects_oversized_cell_during_decode() {
        let huge = "x".repeat(MAX_SCALAR_BYTES as usize + 8);
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/query$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": [{
                    "results": [{"v": huge}],
                    "success": true,
                    "meta": {}
                }]
            })))
            .mount(&server)
            .await;
        let proxy = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        let sql = "SELECT * FROM (SELECT v FROM t) AS _bookclerk_page LIMIT 2 OFFSET 0";
        let err = proxy
            .query(Statement::from_string(DatabaseBackend::Sqlite, sql))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(&MAX_SCALAR_BYTES.to_string()) || msg.contains("exceeds"),
            "expected decode-time scalar-cap error, got {msg}"
        );
    }

    #[tokio::test]
    async fn query_aborts_http_body_past_page_budget() {
        let huge = "x".repeat(max_d1_http_body_bytes() + 1);
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/query$"))
            .respond_with(ResponseTemplate::new(200).set_body_string(huge))
            .mount(&server)
            .await;
        let proxy = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        let err = proxy
            .query(Statement::from_string(DatabaseBackend::Sqlite, "SELECT 1"))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("exceeds"),
            "expected incremental body cap, got {msg}"
        );
    }

    #[test]
    fn d1_http_body_ceiling_is_page_budget_plus_envelope() {
        assert_eq!(
            max_d1_http_body_bytes(),
            MAX_SCALAR_BYTES as usize + D1_JSON_ENVELOPE_BYTES
        );
        assert!(max_d1_http_body_bytes() < (MAX_SCALAR_BYTES as usize).saturating_mul(2));
    }
}
