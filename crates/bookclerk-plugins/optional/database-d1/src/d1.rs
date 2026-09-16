//! Cloudflare D1 HTTP API proxy for SeaORM.
//!
//! D1 HTTP requests do not keep an interactive SQL transaction open.
//! Autocommit statements use the documented `{ "sql", "params" }` REST body.
//! Interactive `BEGIN` is rejected: Time Travel is not a substitute for rollback, and
//! mid-transaction SeaORM reads cannot be satisfied without committing.
//! Atomic library operations use [`D1Proxy::run_typed_atomic`], which sends
//! `{ "batch": [...] }` (a real SQL transaction) with control flow
//! encoded in SQL and a durable `operationId` receipt.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bookclerk_plugin_abi::DbType;

use async_trait::async_trait;
use base64::Engine as _;
use bookclerk_db_exec::{
    b64_string_to_bytes, bytes_to_b64_string, is_txn_broken, note_begin_failed, txn_broken_err,
};
use bookclerk_plugin_sdk::MAX_SCALAR_BYTES;
use sea_orm::{
    Database, DatabaseConnection, DbBackend, DbErr, ProxyDatabaseTrait, ProxyExecResult, ProxyRow,
    Statement, Value,
};
use serde_json::{json, Value as JsonValue};
use tokio::sync::Mutex as AsyncMutex;

/// Open Cloudflare D1 with an explicit API token (host-mediated).
///
/// Connects and pings only. The host applies schema after capability negotiation.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn open(
    api_base: String,
    account_id: String,
    database_id: String,
    token: String,
) -> std::result::Result<DatabaseConnection, DbErr> {
    let proxy = D1Proxy::new(api_base, account_id, database_id, token);
    set_shared_proxy(proxy.clone());
    let db = Database::connect_proxy(DbBackend::Sqlite, Arc::new(Box::new(proxy.clone()))).await?;
    db.ping().await?;
    tracing::debug!(plugin = "d1", "opened library database");
    Ok(db)
}

/// True when `url` targets a loopback host (wiremock in unit tests).
fn host_is_loopback(url: &reqwest::Url) -> bool {
    match url.host_str() {
        Some("localhost") | Some("127.0.0.1") | Some("::1") => true,
        Some(host) => host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()),
        None => false,
    }
}

/// Parses a D1 management URL and refuses cleartext transports off-loopback.
///
/// Production `api_base` is HTTPS (`https://api.cloudflare.com/client/v4`).
/// Loopback HTTP is allowed only in tests so wiremock can stand in.
fn d1_management_url(
    api_base: &str,
    path_and_query: &str,
) -> std::result::Result<reqwest::Url, DbErr> {
    let base = api_base.trim_end_matches('/');
    let parsed = reqwest::Url::parse(&format!("{base}{path_and_query}"))
        .map_err(|e| DbErr::Custom(format!("d1 url: {e}")))?;
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" if cfg!(test) && host_is_loopback(&parsed) => Ok(parsed),
        scheme => Err(DbErr::Custom(format!(
            "d1 api_base must be https (got {scheme})"
        ))),
    }
}

/// Sends one D1 management HTTP request. `url` is already HTTPS (or loopback HTTP in tests).
async fn d1_management_send(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: reqwest::Url,
    token: &str,
    body: Option<JsonValue>,
) -> std::result::Result<reqwest::Response, DbErr> {
    // Cloudflare account ids are public REST path segments. Off-loopback
    // transport is HTTPS (`d1_management_url`); the API token is Bearer, not a
    // URL query. Loopback HTTP is test-only (wiremock).
    // lgtm[rust/cleartext-transmission]
    let mut req = client.request(method, url).bearer_auth(token);
    if let Some(body) = body {
        req = req.json(&body);
    }
    req.send()
        .await
        .map_err(|e| DbErr::Custom(format!("d1 request: {e}")))
}

/// Sends a D1 management request and parses the JSON body.
async fn d1_management_json(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: reqwest::Url,
    token: &str,
    body: Option<JsonValue>,
) -> std::result::Result<(reqwest::StatusCode, JsonValue), DbErr> {
    let response = match url.scheme() {
        "https" => d1_management_send(client, method, url, token, body).await?,
        "http" if cfg!(test) && host_is_loopback(&url) => {
            d1_management_send(client, method, url, token, body).await?
        }
        scheme => {
            return Err(DbErr::Custom(format!(
                "d1 api_base must be https (got {scheme})"
            )));
        }
    };
    let status = response.status();
    let json = response
        .json()
        .await
        .map_err(|e| DbErr::Custom(format!("d1 request: {e}")))?;
    Ok((status, json))
}

/// HTTP client for D1 account-management calls (list/create/delete).
fn d1_management_client() -> std::result::Result<reqwest::Client, DbErr> {
    reqwest::Client::builder()
        .timeout(D1_REQUEST_TIMEOUT)
        .connect_timeout(D1_CONNECT_TIMEOUT)
        .build()
        .map_err(|e| DbErr::Custom(format!("d1 client: {e}")))
}

/// Resolves (and provisions) a Cloudflare D1 database by name, returning its UUID.
///
/// Used for named plugin database bindings: each binding gets its own D1
/// database. Fails closed with an operator-facing error when the API token
/// cannot list or create databases.
///
/// # Errors
///
/// Returns when the HTTP lookup/create fails or the response is malformed.
pub async fn ensure_database(
    api_base: &str,
    account_id: &str,
    api_token: &str,
    name: &str,
) -> std::result::Result<String, DbErr> {
    let client = d1_management_client()?;
    let list_url = d1_management_url(
        api_base,
        &format!("/accounts/{account_id}/d1/database?name={name}"),
    )?;
    let (_status, listed) =
        d1_management_json(&client, reqwest::Method::GET, list_url, api_token, None)
            .await
            .map_err(|e| DbErr::Custom(format!("d1 database lookup `{name}`: {e}")))?;
    if let Some(uuid) = d1_database_uuid_by_name(&listed, name) {
        return Ok(uuid);
    }
    let create_url = d1_management_url(api_base, &format!("/accounts/{account_id}/d1/database"))?;
    let (_status, created) = d1_management_json(
        &client,
        reqwest::Method::POST,
        create_url,
        api_token,
        Some(json!({ "name": name })),
    )
    .await
    .map_err(|e| DbErr::Custom(format!("d1 database create `{name}`: {e}")))?;
    if created.get("success").and_then(JsonValue::as_bool) == Some(false) {
        return Err(DbErr::Custom(format!(
            "d1 database create `{name}` rejected (token may lack D1 edit permission): {}",
            created.get("errors").cloned().unwrap_or(JsonValue::Null)
        )));
    }
    created
        .get("result")
        .and_then(|r| r.get("uuid"))
        .and_then(JsonValue::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            DbErr::Custom(format!(
                "d1 database create `{name}` returned no uuid (token may lack D1 edit permission)"
            ))
        })
}

/// Deletes a Cloudflare D1 database by name. Missing databases are success.
///
/// Used by `bookclerk plugins db drop` so the physical unit is gone before the
/// `plugin_databases` registry row is removed. Does not create-if-missing.
///
/// # Errors
///
/// Returns when lookup or delete fails, or the token lacks D1 edit permission.
pub async fn delete_database(
    api_base: &str,
    account_id: &str,
    api_token: &str,
    name: &str,
) -> std::result::Result<(), DbErr> {
    let client = d1_management_client()?;
    let list_url = d1_management_url(
        api_base,
        &format!("/accounts/{account_id}/d1/database?name={name}"),
    )?;
    let (_status, listed) =
        d1_management_json(&client, reqwest::Method::GET, list_url, api_token, None)
            .await
            .map_err(|e| DbErr::Custom(format!("d1 database lookup `{name}`: {e}")))?;
    let Some(uuid) = d1_database_uuid_by_name(&listed, name) else {
        return Ok(());
    };
    let delete_url = d1_management_url(
        api_base,
        &format!("/accounts/{account_id}/d1/database/{uuid}"),
    )?;
    match delete_url.scheme() {
        "https" => {}
        "http" if cfg!(test) && host_is_loopback(&delete_url) => {}
        scheme => {
            return Err(DbErr::Custom(format!(
                "d1 api_base must be https (got {scheme})"
            )));
        }
    }
    let deleted = d1_management_send(
        &client,
        reqwest::Method::DELETE,
        delete_url,
        api_token,
        None,
    )
    .await
    .map_err(|e| DbErr::Custom(format!("d1 database delete `{name}`: {e}")))?;
    let status = deleted.status();
    if status.as_u16() == 404 {
        return Ok(());
    }
    let body: JsonValue = deleted
        .json()
        .await
        .map_err(|e| DbErr::Custom(format!("d1 database delete `{name}`: {e}")))?;
    if body.get("success").and_then(JsonValue::as_bool) == Some(false) {
        return Err(DbErr::Custom(format!(
            "d1 database delete `{name}` rejected (token may lack D1 edit permission): {}",
            body.get("errors").cloned().unwrap_or(JsonValue::Null)
        )));
    }
    if !status.is_success() {
        return Err(DbErr::Custom(format!(
            "d1 database delete `{name}` HTTP {status}"
        )));
    }
    Ok(())
}

/// Extracts the UUID of a D1 database named exactly `name` from a list reply.
fn d1_database_uuid_by_name(listed: &JsonValue, name: &str) -> Option<String> {
    listed
        .get("result")?
        .as_array()?
        .iter()
        .find(|entry| entry.get("name").and_then(JsonValue::as_str) == Some(name))?
        .get("uuid")?
        .as_str()
        .map(str::to_string)
}

/// Operator-facing reason recorded when SeaORM `begin` is rejected on D1 HTTP.
const D1_INTERACTIVE_TXN_UNSUPPORTED: &str = "D1 does not support interactive transactions; \
     each HTTP request commits immediately. Use atomic execute for claim redeem, last-owner guards, and consume-once tokens";

/// Process-wide D1 proxy installed by [`open`] so atomic execute can reuse the same HTTP client.
static SHARED: OnceLock<D1Proxy> = OnceLock::new();

/// Stores the process-wide D1 proxy used by atomic execute after [`open`].
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
    /// Declared column types per table (`pragma_table_info`), lowercased
    /// keys. Used for universal result normalization; cleared on DDL.
    table_types: std::sync::Mutex<HashMap<String, HashMap<String, DbType>>>,
    /// Test-only: wait here before sending a guest-receipt claim INSERT.
    #[cfg(test)]
    claim_pause: std::sync::Mutex<Option<Arc<tokio::sync::Barrier>>>,
    /// Test-only: after a won claim INSERT, fail before guest DDL.
    #[cfg(test)]
    fail_after_won_claim: std::sync::atomic::AtomicBool,
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
pub(crate) const D1_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
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
    /// Guest-visible `deadlineUnixMs` elapsed (including HTTP mutex wait).
    Deadline {
        /// Operator-facing deadline text.
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

    /// Guest-visible deadline elapsed.
    fn deadline(message: impl Into<String>) -> Self {
        Self::Deadline {
            message: message.into(),
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
            Self::Permanent { .. } | Self::Deadline { .. } => None,
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
            D1Error::Deadline { message } => DbErr::Custom(message),
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
                table_types: std::sync::Mutex::new(HashMap::new()),
                #[cfg(test)]
                claim_pause: std::sync::Mutex::new(None),
                #[cfg(test)]
                fail_after_won_claim: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    /// Test helper: both racers wait here before the claim INSERT HTTP.
    #[cfg(test)]
    pub fn pause_claims(&self, barrier: Arc<tokio::sync::Barrier>) {
        *self
            .inner
            .claim_pause
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(barrier);
    }

    /// Waits on [`Self::pause_claims`] when a test armed a barrier.
    #[cfg(test)]
    pub(crate) async fn maybe_pause_claim(&self) {
        let barrier = self
            .inner
            .claim_pause
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(barrier) = barrier {
            barrier.wait().await;
        }
    }

    /// Test helper: next won claim INSERT returns unavailable before guest DDL.
    #[cfg(test)]
    pub fn fail_next_won_claim(&self) {
        self.inner
            .fail_after_won_claim
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Consumes [`Self::fail_next_won_claim`] when a test armed a post-claim crash.
    #[cfg(test)]
    pub(crate) fn consume_fail_after_won_claim(&self) -> bool {
        self.inner
            .fail_after_won_claim
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    /// Cached declared column types for a (lowercased) table name.
    pub(crate) fn cached_table_types(&self, table: &str) -> Option<HashMap<String, DbType>> {
        self.inner
            .table_types
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(table)
            .cloned()
    }

    /// Stores declared column types for a (lowercased) table name.
    pub(crate) fn store_table_types(&self, table: String, columns: HashMap<String, DbType>) {
        self.inner
            .table_types
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(table, columns);
    }

    /// Clears the declared-type cache (called when a batch contains DDL).
    pub(crate) fn clear_table_types(&self) {
        self.inner
            .table_types
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
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
        self.post_json_timed(url, body, D1_REQUEST_TIMEOUT).await
    }

    /// POSTs a JSON body with an explicit HTTP timeout.
    ///
    /// The timeout covers HTTP mutex acquire plus send so a queued caller
    /// cannot outlive `deadlineUnixMs` while waiting for the slot.
    async fn post_json_timed(
        &self,
        url: &str,
        body: JsonValue,
        timeout: Duration,
    ) -> std::result::Result<JsonValue, D1Error> {
        let timeout = timeout.min(D1_REQUEST_TIMEOUT);
        if timeout.is_zero() {
            return Err(D1Error::deadline(
                "deadline_exceeded: atomic deadline elapsed",
            ));
        }
        let deadline = std::time::Instant::now() + timeout;
        let send = async {
            let _http = self.inner.http.lock().await;
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(D1Error::deadline(
                    "deadline_exceeded: atomic deadline elapsed waiting for D1 HTTP slot",
                ));
            }
            let response = self
                .inner
                .client
                .post(url)
                .bearer_auth(&self.inner.api_token)
                .timeout(remaining)
                .json(&body)
                .send()
                .await
                .map_err(|e| D1Error::ambiguous(format!("transport: {e}")))?;
            parse_d1_response(response).await
        };
        match tokio::time::timeout(timeout, send).await {
            Ok(result) => result,
            Err(_) => Err(D1Error::deadline(
                "deadline_exceeded: atomic deadline elapsed waiting for D1 HTTP slot",
            )),
        }
    }

    /// Holds the serialized D1 HTTP slot (tests only).
    #[cfg(test)]
    pub(crate) async fn lock_http_for_test(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.inner.http.lock().await
    }

    /// Runs one or more SQL statements as one documented D1 `{ "batch": [...] }`
    /// request: statements execute sequentially as one SQL transaction and roll
    /// back together on failure.
    #[cfg(test)]
    pub(crate) async fn run_batch(
        &self,
        statements: &[(String, Vec<JsonValue>)],
    ) -> std::result::Result<JsonValue, D1Error> {
        self.run_batch_with_timeout(statements, D1_REQUEST_TIMEOUT)
            .await
    }

    /// [`run_batch`] with a caller-supplied HTTP timeout.
    pub(crate) async fn run_batch_with_timeout(
        &self,
        statements: &[(String, Vec<JsonValue>)],
        timeout: Duration,
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
        self.post_json_timed(&self.query_url(), body, timeout).await
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
        if is_txn_broken() {
            return Err(txn_broken_err());
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
        if is_txn_broken() {
            return Err(txn_broken_err());
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
        note_begin_failed(D1_INTERACTIVE_TXN_UNSUPPORTED);
    }

    async fn commit(&self) {}

    async fn rollback(&self) {}

    fn start_rollback(&self) {}

    async fn ping(&self) -> std::result::Result<(), DbErr> {
        if is_txn_broken() {
            return Err(txn_broken_err());
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
            return Err(body_cap_error(
                response.status(),
                format!("D1 body {len} bytes exceeds {max}"),
            ));
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
            return Err(body_cap_error(
                response.status(),
                format!("D1 body exceeds {max}"),
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Oversized bodies after a successful or retryable HTTP status are ambiguous.
fn body_cap_error(status: reqwest::StatusCode, message: String) -> D1Error {
    if status.is_success() || retryable_http_status(status) {
        D1Error::ambiguous(message)
    } else {
        D1Error::Permanent {
            status: status.as_u16(),
            message,
        }
    }
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

/// Maps a D1 JSON cell back to a SeaORM [`Value`].
///
/// `b64:` decoding applies only to known blob columns (`ciphertext`,
/// `kdf_salt`, `cipher_nonce`, `vector`). Ordinary TEXT stays a string even
/// when it starts with a decodable `b64:` prefix.
///
/// String and blob cells larger than [`MAX_SCALAR_BYTES`] are rejected before
/// clone or base64 decode.
fn json_to_sea_value(v: &JsonValue, column: &str) -> std::result::Result<Value, DbErr> {
    reject_oversized_json_cell(v, column)?;
    match v {
        JsonValue::Null => Ok(bookclerk_plugin_sdk::database_adapter::typed_null(
            None, column,
        )),
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
            if is_binary_column(column) {
                if let Some(bytes) = b64_string_to_bytes(s) {
                    if bytes.len() > MAX_SCALAR_BYTES as usize {
                        return Err(oversized_cell_err(column, bytes.len()));
                    }
                    return Ok(Value::Bytes(Some(bytes)));
                }
                // Legacy: try base64 without prefix (shouldn't happen with new writes).
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(s.as_bytes()) {
                    if bytes.len() > MAX_SCALAR_BYTES as usize {
                        return Err(oversized_cell_err(column, bytes.len()));
                    }
                    return Ok(Value::Bytes(Some(bytes)));
                }
                return Ok(Value::Bytes(Some(s.as_bytes().to_vec())));
            }
            Ok(Value::String(Some(s.clone())))
        }
        JsonValue::Array(cells) => {
            // Real D1 returns BLOB cells as JSON arrays of byte integers.
            let bytes: Option<Vec<u8>> = cells
                .iter()
                .map(|c| c.as_u64().and_then(|n| u8::try_from(n).ok()))
                .collect();
            if let Some(bytes) = bytes {
                if bytes.len() > MAX_SCALAR_BYTES as usize {
                    return Err(oversized_cell_err(column, bytes.len()));
                }
                return Ok(Value::Bytes(Some(bytes)));
            }
            let encoded = v.to_string();
            if encoded.len() > MAX_SCALAR_BYTES as usize {
                return Err(oversized_cell_err(column, encoded.len()));
            }
            Ok(Value::String(Some(encoded)))
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

    fn named_plan(
        operation_id: &str,
        operation: bookclerk_library::DbAtomicParams,
    ) -> bookclerk_plugin_abi::ExecuteRequest {
        let now = chrono::Utc::now().to_rfc3339();
        bookclerk_library::compile_named_request(operation_id, &operation, &now)
            .expect("compile named atomic request")
            .into_typed_request(operation_id)
    }

    async fn run_named_atomic(
        proxy: &D1Proxy,
        req: bookclerk_plugin_abi::ExecuteRequest,
    ) -> std::result::Result<bookclerk_plugin_abi::ExecuteReply, sea_orm::DbErr> {
        proxy
            .run_typed_atomic(
                &req,
                bookclerk_plugin_abi::GuestReceiptPersist::default(),
                &[],
            )
            .await
    }

    fn typed_sql_req(
        op: &str,
        statements: Vec<(&str, bookclerk_plugin_sdk::DbPlanStatementKind)>,
        deadline_unix_ms: u64,
    ) -> bookclerk_plugin_abi::ExecuteRequest {
        use bookclerk_plugin_abi::{DbPlanStatementKind, DbResultSelection, TypedDbStatement};
        bookclerk_plugin_abi::ExecuteRequest {
            operation_id: op.into(),
            request_hash: String::new(),
            statements: statements
                .into_iter()
                .map(|(sql, kind)| TypedDbStatement {
                    sql: sql.into(),
                    parameters: vec![],
                    kind,
                    max_rows: 0,
                    result_selection: if kind == DbPlanStatementKind::Execute {
                        DbResultSelection::AffectedRows
                    } else {
                        DbResultSelection::Rows
                    },
                })
                .collect(),
            deadline_unix_ms,
        }
    }

    fn is_declared_type_query(body: &JsonValue) -> bool {
        body["batch"].as_array().is_some_and(|batch| {
            !batch.is_empty()
                && batch.iter().all(|stmt| {
                    stmt["sql"]
                        .as_str()
                        .is_some_and(|sql| sql.to_ascii_lowercase().contains("pragma_table_info"))
                })
        })
    }

    fn atomic_http_batches(queries: &[JsonValue]) -> Vec<&JsonValue> {
        queries
            .iter()
            .filter(|q| !is_declared_type_query(q))
            .collect()
    }

    fn request_is_declared_types(request: &wiremock::Request) -> bool {
        std::str::from_utf8(&request.body)
            .unwrap_or("")
            .to_ascii_lowercase()
            .contains("pragma_table_info")
    }

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
    async fn ensure_database_resolves_existing_by_name() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/accounts/acct/d1/database$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": [
                    { "name": "other", "uuid": "not-this-one" },
                    { "name": "bookclerk-pb-demo-db", "uuid": "uuid-existing" }
                ]
            })))
            .mount(&server)
            .await;
        let id = ensure_database(&server.uri(), "acct", "token", "bookclerk-pb-demo-db")
            .await
            .expect("resolve existing database");
        assert_eq!(id, "uuid-existing");
    }

    #[tokio::test]
    async fn ensure_database_creates_when_missing_and_fails_closed_without_permission() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/accounts/acct/d1/database$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "success": true, "result": [] })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/accounts/acct/d1/database$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": { "name": "bookclerk-pb-demo-db", "uuid": "uuid-created" }
            })))
            .mount(&server)
            .await;
        let id = ensure_database(&server.uri(), "acct", "token", "bookclerk-pb-demo-db")
            .await
            .expect("create missing database");
        assert_eq!(id, "uuid-created");

        // A token without D1 edit permission must fail closed with an
        // operator-facing reason, never fall through to a bogus id.
        let denied = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/accounts/acct/d1/database$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "success": true, "result": [] })),
            )
            .mount(&denied)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/accounts/acct/d1/database$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": false,
                "errors": [{ "code": 10000, "message": "Authentication error" }]
            })))
            .mount(&denied)
            .await;
        let err = ensure_database(&denied.uri(), "acct", "token", "bookclerk-pb-demo-db")
            .await
            .expect_err("create without permission fails closed");
        assert!(err.to_string().contains("permission"), "{err}");
    }

    #[tokio::test]
    async fn d1_management_refuses_cleartext_remote() {
        let err = ensure_database(
            "http://example.com",
            "acct",
            "token",
            "bookclerk-pb-demo-db",
        )
        .await
        .expect_err("remote http must fail closed");
        assert!(err.to_string().contains("https"), "{err}");
        let err = delete_database(
            "http://example.com",
            "acct",
            "token",
            "bookclerk-pb-demo-db",
        )
        .await
        .expect_err("remote http delete must fail closed");
        assert!(err.to_string().contains("https"), "{err}");
    }

    #[tokio::test]
    async fn delete_database_removes_by_uuid_and_treats_missing_as_success() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/accounts/acct/d1/database$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": [
                    { "name": "bookclerk-pb-demo-db", "uuid": "uuid-to-delete" }
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path_regex(r"^/accounts/acct/d1/database/uuid-to-delete$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": { "uuid": "uuid-to-delete" }
            })))
            .mount(&server)
            .await;
        delete_database(&server.uri(), "acct", "token", "bookclerk-pb-demo-db")
            .await
            .expect("delete existing database");

        let missing = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/accounts/acct/d1/database$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "success": true, "result": [] })),
            )
            .mount(&missing)
            .await;
        delete_database(&missing.uri(), "acct", "token", "bookclerk-pb-demo-db")
            .await
            .expect("missing database is already gone");
    }

    #[tokio::test]
    async fn delete_database_fails_closed_without_permission() {
        let denied = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/accounts/acct/d1/database$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": [{ "name": "bookclerk-pb-demo-db", "uuid": "uuid-x" }]
            })))
            .mount(&denied)
            .await;
        Mock::given(method("DELETE"))
            .and(path_regex(r"^/accounts/acct/d1/database/uuid-x$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": false,
                "errors": [{ "code": 10000, "message": "Authentication error" }]
            })))
            .mount(&denied)
            .await;
        let err = delete_database(&denied.uri(), "acct", "token", "bookclerk-pb-demo-db")
            .await
            .expect_err("delete without permission fails closed");
        assert!(err.to_string().contains("permission"), "{err}");
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
        use bookclerk_library::DbAtomicParams;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/query$"))
            .respond_with(EchoBatchOk)
            .mount(&server)
            .await;

        let proxy = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        let _ = run_named_atomic(
            &proxy,
            named_plan(
                "op-redeem",
                DbAtomicParams::RedeemClaimTicket {
                    token_hash: "ticket".into(),
                    session_hash: "session".into(),
                    expires_at: "2099-01-01T00:00:00Z".into(),
                    user_agent: None,
                    device_type: None,
                    client_label: None,
                    new_password_hash: Some("hash".into()),
                    password_fingerprint: Some("fp".into()),
                },
            ),
        )
        .await;

        let queries: Vec<JsonValue> = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path().ends_with("/query"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        let atomic = atomic_http_batches(&queries);
        assert_eq!(atomic.len(), 1, "{queries:?}");
        let batch = atomic[0]["batch"]
            .as_array()
            .expect("atomic execute must use the documented { batch: [...] } envelope");
        assert!(
            batch.len() > 1,
            "atomic execute must send a multi-statement D1 batch, got {}",
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
        use bookclerk_library::DbAtomicParams;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/query$"))
            .respond_with(EchoBatchOk)
            .mount(&server)
            .await;

        let proxy = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        let _ = run_named_atomic(
            &proxy,
            named_plan(
                "op-totp",
                DbAtomicParams::ConfirmTotpEnrollment {
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
            ),
        )
        .await;

        let queries: Vec<JsonValue> = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path().ends_with("/query"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        let atomic = atomic_http_batches(&queries);
        assert_eq!(atomic.len(), 1, "{queries:?}");
        let batch = atomic[0]["batch"]
            .as_array()
            .expect("atomic execute must use the documented { batch: [...] } envelope");
        assert!(
            batch.len() > 1,
            "atomic execute must send a multi-statement D1 batch, got {}",
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
        let body = serde_json::to_string(atomic[0]).unwrap();
        assert!(
            !body.contains("$sea_null"),
            "D1 HTTP params must flatten typed nulls to JSON null: {body}"
        );
    }

    #[tokio::test]
    async fn atomic_take_oidc_posts_delete_returning_batch() {
        use bookclerk_library::DbAtomicParams;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/query$"))
            .respond_with(EchoBatchOk)
            .mount(&server)
            .await;

        let proxy = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        let _ = run_named_atomic(
            &proxy,
            named_plan(
                "op-take",
                DbAtomicParams::TakeOidcRpState {
                    state_hash: "abc".into(),
                },
            ),
        )
        .await;

        let queries: Vec<JsonValue> = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path().ends_with("/query"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        let atomic = atomic_http_batches(&queries);
        assert_eq!(atomic.len(), 1, "{queries:?}");
        let batch = atomic[0]["batch"]
            .as_array()
            .expect("atomic execute must use the documented { batch: [...] } envelope");
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
        use bookclerk_library::DbAtomicParams;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use wiremock::Respond;

        struct FirstMangledThenOk {
            hits: Arc<AtomicUsize>,
        }
        impl Respond for FirstMangledThenOk {
            fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
                if request_is_declared_types(request) {
                    return d1_success_for_batch_request(request);
                }
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
        let result = run_named_atomic(
            &proxy,
            named_plan(
                "op-retry",
                DbAtomicParams::TakeOidcRpState {
                    state_hash: "abc".into(),
                },
            ),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn atomic_take_oidc_retries_incomplete_2xx() {
        use bookclerk_library::DbAtomicParams;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use wiremock::Respond;

        struct FirstIncompleteThenOk {
            hits: Arc<AtomicUsize>,
        }
        impl Respond for FirstIncompleteThenOk {
            fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
                if request_is_declared_types(request) {
                    return d1_success_for_batch_request(request);
                }
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
        let result = run_named_atomic(
            &proxy,
            named_plan(
                "op-incomplete",
                DbAtomicParams::TakeOidcRpState {
                    state_hash: "abc".into(),
                },
            ),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn atomic_take_oidc_outer_retry_reuses_operation_id_after_two_lost_replies() {
        use bookclerk_library::DbAtomicParams;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use wiremock::Respond;

        struct TwoIncompleteThenOk {
            hits: Arc<AtomicUsize>,
        }
        impl Respond for TwoIncompleteThenOk {
            fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
                if request_is_declared_types(request) {
                    return d1_success_for_batch_request(request);
                }
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
        let req = named_plan(
            "op-outer",
            DbAtomicParams::TakeOidcRpState {
                state_hash: "abc".into(),
            },
        );
        // Inner loop retries twice on incomplete 2xx, then succeeds on the third
        // attempt with the same operation_id (the outer caller also reuses it).
        let result = run_named_atomic(&proxy, req.clone()).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 3);

        let replay = run_named_atomic(&proxy, req).await.unwrap();
        assert_eq!(replay.operation_id, "op-outer");
        assert!(hits.load(Ordering::SeqCst) >= 4);
    }

    #[tokio::test]
    async fn atomic_take_oidc_exhausted_inner_retries_then_same_id_recovers() {
        use bookclerk_library::DbAtomicParams;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use wiremock::Respond;

        struct ThreeIncompleteThenOk {
            hits: Arc<AtomicUsize>,
        }
        impl Respond for ThreeIncompleteThenOk {
            fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
                if request_is_declared_types(request) {
                    return d1_success_for_batch_request(request);
                }
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
        let req = named_plan(
            "op-resume",
            DbAtomicParams::TakeOidcRpState {
                state_hash: "abc".into(),
            },
        );
        let first = run_named_atomic(&proxy, req.clone()).await;
        assert!(first.is_err(), "three incomplete 2xx exhaust inner retries");
        assert_eq!(hits.load(Ordering::SeqCst), 3);

        let recovered = run_named_atomic(&proxy, req).await;
        assert!(recovered.is_ok(), "{recovered:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn atomic_permanent_400_is_not_retried() {
        use bookclerk_library::DbAtomicParams;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use wiremock::Respond;

        struct Always400 {
            hits: Arc<AtomicUsize>,
        }
        impl Respond for Always400 {
            fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
                if request_is_declared_types(request) {
                    return d1_success_for_batch_request(request);
                }
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
        let err = run_named_atomic(
            &proxy,
            named_plan(
                "op-400",
                DbAtomicParams::TakeOidcRpState {
                    state_hash: "abc".into(),
                },
            ),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("D1 HTTP 400"), "{err}");
        assert!(!crate::atomic::is_ambiguous_d1(&err), "{err}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn atomic_503_is_retried_then_succeeds() {
        use bookclerk_library::DbAtomicParams;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use wiremock::Respond;

        struct First503ThenOk {
            hits: Arc<AtomicUsize>,
        }
        impl Respond for First503ThenOk {
            fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
                if request_is_declared_types(request) {
                    return d1_success_for_batch_request(request);
                }
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
        let result = run_named_atomic(
            &proxy,
            named_plan(
                "op-503",
                DbAtomicParams::TakeOidcRpState {
                    state_hash: "abc".into(),
                },
            ),
        )
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
    fn json_to_sea_value_keeps_decodable_b64_text_unless_blob_column() {
        let text = json_to_sea_value(&JsonValue::String("b64:YWJj".into()), "note").unwrap();
        assert_eq!(text, Value::String(Some("b64:YWJj".into())));
        let blob = json_to_sea_value(&JsonValue::String("b64:AA==".into()), "ciphertext").unwrap();
        assert_eq!(blob, Value::Bytes(Some(vec![0])));
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
            bookclerk_db_guest::guest_sql("SELECT id FROM t"),
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

    struct ExecutingD1 {
        conn: std::sync::Arc<std::sync::Mutex<rusqlite::Connection>>,
        /// Remaining BetweenStatements skips before a simulated crash (`u32::MAX` = off).
        interrupt_after: std::sync::Arc<std::sync::atomic::AtomicU32>,
        /// When true, COMMIT then return HTTP 500 (committed but reply lost).
        drop_reply_after_commit: std::sync::Arc<std::sync::atomic::AtomicBool>,
        /// When non-zero, COMMIT then return this status with a body over the HTTP cap.
        oversized_body_status: std::sync::Arc<std::sync::atomic::AtomicU16>,
        /// When true, a `pragma_table_info` batch returns HTTP 500 after COMMIT.
        fail_pragma_table_info: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl wiremock::Respond for ExecutingD1 {
        fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
            let body: JsonValue = serde_json::from_slice(&request.body).unwrap_or(json!({}));
            let conn = self.conn.lock().expect("sqlite mutex");
            if let Some(batch) = body.get("batch").and_then(JsonValue::as_array) {
                return sqlite_exec_batch(
                    &conn,
                    &self.interrupt_after,
                    &self.drop_reply_after_commit,
                    &self.oversized_body_status,
                    &self.fail_pragma_table_info,
                    batch,
                );
            }
            if let Some(sql) = body.get("sql").and_then(JsonValue::as_str) {
                let params = body
                    .get("params")
                    .and_then(JsonValue::as_array)
                    .cloned()
                    .unwrap_or_default();
                return sqlite_exec_one(&conn, sql, &params);
            }
            ResponseTemplate::new(400).set_body_json(json!({
                "success": false,
                "errors": [{"message": "missing sql"}]
            }))
        }
    }

    fn json_to_rusqlite(v: &JsonValue) -> rusqlite::types::Value {
        match v {
            JsonValue::Null => rusqlite::types::Value::Null,
            JsonValue::Bool(b) => rusqlite::types::Value::Integer(i64::from(*b)),
            JsonValue::Number(n) => {
                if let Some(i) = n.as_i64() {
                    rusqlite::types::Value::Integer(i)
                } else if let Some(u) = n.as_u64() {
                    rusqlite::types::Value::Integer(i64::try_from(u).unwrap_or(i64::MAX))
                } else {
                    rusqlite::types::Value::Real(n.as_f64().unwrap_or(0.0))
                }
            }
            JsonValue::String(s) => rusqlite::types::Value::Text(s.clone()),
            other => rusqlite::types::Value::Text(other.to_string()),
        }
    }

    fn sqlite_exec_one(
        conn: &rusqlite::Connection,
        sql: &str,
        params: &[JsonValue],
    ) -> ResponseTemplate {
        match sqlite_run_statement(conn, sql, params) {
            Ok(entry) => ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": [entry]
            })),
            Err(msg) => ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": [{ "success": false, "error": msg }]
            })),
        }
    }

    /// True when this batch is only a prior-receipt SELECT peek.
    fn batch_is_guest_receipt_peek(batch: &[JsonValue]) -> bool {
        !batch.is_empty()
            && batch.iter().all(|stmt| {
                let sql = stmt
                    .get("sql")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("")
                    .to_ascii_lowercase();
                sql.contains("db_atomic_receipts")
                    && sql.contains("select ")
                    && !sql.contains("insert ")
            })
    }

    /// True when this batch is the guest-receipt claim stub INSERT.
    fn batch_is_guest_receipt_claim(batch: &[JsonValue]) -> bool {
        batch.len() == 1
            && batch.iter().all(|stmt| {
                let sql = stmt.get("sql").and_then(JsonValue::as_str).unwrap_or("");
                bookclerk_db_exec::is_guest_receipt_stub_insert(sql)
            })
    }

    fn sqlite_exec_batch(
        conn: &rusqlite::Connection,
        interrupt_after: &std::sync::atomic::AtomicU32,
        drop_reply_after_commit: &std::sync::atomic::AtomicBool,
        oversized_body_status: &std::sync::atomic::AtomicU16,
        fail_pragma_table_info: &std::sync::atomic::AtomicBool,
        batch: &[JsonValue],
    ) -> ResponseTemplate {
        if conn.execute_batch("BEGIN IMMEDIATE").is_err() {
            return ResponseTemplate::new(500).set_body_json(json!({
                "success": false,
                "errors": [{"message": "begin failed"}]
            }));
        }
        let mut results = Vec::new();
        for stmt in batch {
            let sql = stmt.get("sql").and_then(JsonValue::as_str).unwrap_or("");
            let params = stmt
                .get("params")
                .and_then(JsonValue::as_array)
                .cloned()
                .unwrap_or_default();
            match sqlite_run_statement(conn, sql, &params) {
                Ok(entry) => {
                    let skips = interrupt_after.load(std::sync::atomic::Ordering::SeqCst);
                    if skips != u32::MAX {
                        if skips == 0 {
                            interrupt_after.store(u32::MAX, std::sync::atomic::Ordering::SeqCst);
                            let _ = conn.execute_batch("ROLLBACK");
                            return ResponseTemplate::new(200).set_body_json(json!({
                                "success": true,
                                "result": [{
                                    "success": false,
                                    "error": "cancelled: atomic session cancelled"
                                }]
                            }));
                        }
                        interrupt_after.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    results.push(entry);
                }
                Err(msg) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    results.push(json!({ "success": false, "error": msg }));
                    return ResponseTemplate::new(200).set_body_json(json!({
                        "success": true,
                        "result": results
                    }));
                }
            }
        }
        if conn.execute_batch("COMMIT").is_err() {
            let _ = conn.execute_batch("ROLLBACK");
            return ResponseTemplate::new(500).set_body_json(json!({
                "success": false,
                "errors": [{"message": "commit failed"}]
            }));
        }
        // Lost-reply injection models a dropped mutating batch, not the
        // read-only prior-receipt SELECT peek or the claim stub INSERT.
        if !batch_is_guest_receipt_peek(batch)
            && !batch_is_guest_receipt_claim(batch)
            && drop_reply_after_commit.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return ResponseTemplate::new(500).set_body_json(json!({
                "success": false,
                "errors": [{"message": "commit reply lost"}]
            }));
        }
        if fail_pragma_table_info.load(std::sync::atomic::Ordering::SeqCst)
            && batch.iter().any(|stmt| {
                stmt.get("sql")
                    .and_then(JsonValue::as_str)
                    .is_some_and(|sql| sql.to_ascii_lowercase().contains("pragma_table_info"))
            })
        {
            fail_pragma_table_info.store(false, std::sync::atomic::Ordering::SeqCst);
            return ResponseTemplate::new(500).set_body_json(json!({
                "success": false,
                "errors": [{"message": "pragma_table_info failed"}]
            }));
        }
        let oversized = oversized_body_status.load(std::sync::atomic::Ordering::SeqCst);
        if oversized != 0 {
            let huge = "x".repeat(max_d1_http_body_bytes() + 1);
            return ResponseTemplate::new(oversized).set_body_string(huge);
        }
        ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "result": results
        }))
    }

    fn sqlite_run_statement(
        conn: &rusqlite::Connection,
        sql: &str,
        params: &[JsonValue],
    ) -> std::result::Result<JsonValue, String> {
        let binds: Vec<rusqlite::types::Value> = params.iter().map(json_to_rusqlite).collect();
        let mut stmt = conn.prepare(sql).map_err(format_exec_sqlite_err)?;
        let names: Vec<String> = (0..stmt.column_count())
            .map(|i| stmt.column_name(i).unwrap_or("").to_string())
            .collect();
        if !names.is_empty() {
            let mut rows = stmt
                .query(rusqlite::params_from_iter(binds.iter()))
                .map_err(format_exec_sqlite_err)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(format_exec_sqlite_err)? {
                let mut map = serde_json::Map::new();
                for (i, name) in names.iter().enumerate() {
                    let json = match row.get_ref(i).ok() {
                        Some(rusqlite::types::ValueRef::Null) | None => JsonValue::Null,
                        Some(rusqlite::types::ValueRef::Integer(n)) => json!(n),
                        Some(rusqlite::types::ValueRef::Real(n)) => json!(n),
                        Some(rusqlite::types::ValueRef::Text(t)) => {
                            JsonValue::String(String::from_utf8_lossy(t).into_owned())
                        }
                        // Real D1 returns BLOB cells as JSON arrays of byte
                        // integers (Cloudflare type-conversion docs).
                        Some(rusqlite::types::ValueRef::Blob(b)) => {
                            JsonValue::Array(b.iter().map(|&x| json!(x)).collect())
                        }
                    };
                    map.insert(name.clone(), json);
                }
                out.push(JsonValue::Object(map));
            }
            return Ok(json!({
                "success": true,
                "results": out,
                "meta": { "changes": 0, "last_row_id": 0, "timings": { "sql_duration_ms": 0.1 } }
            }));
        }
        stmt.execute(rusqlite::params_from_iter(binds.iter()))
            .map_err(format_exec_sqlite_err)?;
        Ok(json!({
            "success": true,
            "results": [],
            "meta": {
                "changes": conn.changes(),
                "last_row_id": conn.last_insert_rowid(),
                "timings": { "sql_duration_ms": 0.1 }
            }
        }))
    }

    fn format_exec_sqlite_err(err: rusqlite::Error) -> String {
        match err {
            rusqlite::Error::SqliteFailure(ffi, msg) => {
                let name = match ffi.code {
                    rusqlite::ErrorCode::ConstraintViolation => "SQLITE_CONSTRAINT",
                    rusqlite::ErrorCode::DatabaseBusy => "SQLITE_BUSY",
                    rusqlite::ErrorCode::DatabaseLocked => "SQLITE_LOCKED",
                    _ => "SQLITE_ERROR",
                };
                match msg {
                    Some(detail) => format!("{name} ({}): {detail}", ffi.extended_code),
                    None => format!("{name} ({})", ffi.extended_code),
                }
            }
            other => other.to_string(),
        }
    }

    async fn executing_proxy_inner() -> (
        MockServer,
        D1Proxy,
        std::sync::Arc<std::sync::Mutex<rusqlite::Connection>>,
        std::sync::Arc<std::sync::atomic::AtomicU32>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::sync::Arc<std::sync::atomic::AtomicU16>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let conn = std::sync::Arc::new(std::sync::Mutex::new(conn));
        let interrupt_after = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(u32::MAX));
        let drop_reply = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let oversized = std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0));
        let fail_pragma = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/query$"))
            .respond_with(ExecutingD1 {
                conn: std::sync::Arc::clone(&conn),
                interrupt_after: std::sync::Arc::clone(&interrupt_after),
                drop_reply_after_commit: std::sync::Arc::clone(&drop_reply),
                oversized_body_status: std::sync::Arc::clone(&oversized),
                fail_pragma_table_info: std::sync::Arc::clone(&fail_pragma),
            })
            .mount(&server)
            .await;
        let proxy = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        (
            server,
            proxy,
            conn,
            interrupt_after,
            drop_reply,
            oversized,
            fail_pragma,
        )
    }

    async fn executing_proxy() -> (
        MockServer,
        D1Proxy,
        std::sync::Arc<std::sync::Mutex<rusqlite::Connection>>,
        std::sync::Arc<std::sync::atomic::AtomicU32>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::sync::Arc<std::sync::atomic::AtomicU16>,
    ) {
        let (server, proxy, conn, interrupt, drop, oversized, _pragma) =
            executing_proxy_inner().await;
        (server, proxy, conn, interrupt, drop, oversized)
    }

    /// Reloads `bookclerk_sql_catalog` into the guest policy (host binding path).
    async fn execute_d1_guest_atomic(
        db: &sea_orm::DatabaseConnection,
        proxy: D1Proxy,
        req: bookclerk_plugin_abi::ExecuteRequest,
    ) -> Result<bookclerk_plugin_abi::ExecuteReply, bookclerk_plugin_abi::PluginError> {
        let caps = bookclerk_plugin_abi::DbCapabilities::advertised_d1();
        let env = bookclerk_db_exec::load_sql_type_env(db)
            .await
            .expect("load binding catalog");
        let policy = bookclerk_library::GuestSqlPolicy::binding_owned().with_sql_types(env);
        bookclerk_library::execute_guest_atomic_with(req, &caps, &policy, |envelope| {
            let proxy = proxy.clone();
            async move {
                proxy
                    .run_typed_atomic(&envelope.request, envelope.guest_receipt, &envelope.proofs)
                    .await
                    .map_err(crate::atomic::plugin_error_from_d1)
            }
        })
        .await
    }

    #[tokio::test]
    async fn plugin_error_from_d1_maps_guest_receipt_result_lost_to_unavailable() {
        let err = DbErr::Custom(bookclerk_db_exec::GUEST_RECEIPT_RESULT_LOST.into());
        let mapped = crate::atomic::plugin_error_from_d1(err);
        assert_eq!(
            mapped.code,
            bookclerk_plugin_sdk::PluginErrorCode::Unavailable,
            "{mapped}"
        );
    }

    #[tokio::test]
    async fn plugin_error_from_d1_maps_post_commit_pragma_failure_to_unavailable() {
        let err = DbErr::Custom(
            "unavailable: declared types for t could not be loaded: D1 HTTP 500".into(),
        );
        let mapped = crate::atomic::plugin_error_from_d1(err);
        assert_eq!(
            mapped.code,
            bookclerk_plugin_sdk::PluginErrorCode::Unavailable,
            "{mapped}"
        );
    }

    #[tokio::test]
    async fn executing_mock_unique_constraint_fails_closed() {
        let (_server, proxy, _conn, _interrupt, _drop, _oversize) = executing_proxy().await;
        proxy
            .run_batch(&[("CREATE TABLE t (k TEXT PRIMARY KEY)".into(), Vec::new())])
            .await
            .unwrap();
        let req = typed_sql_req(
            "dup",
            vec![
                (
                    "INSERT INTO t (k) VALUES ('a')",
                    bookclerk_plugin_sdk::DbPlanStatementKind::Execute,
                ),
                (
                    "INSERT INTO t (k) VALUES ('a')",
                    bookclerk_plugin_sdk::DbPlanStatementKind::Execute,
                ),
            ],
            0,
        );
        let err = run_named_atomic(&proxy, req).await.unwrap_err();
        let mapped = crate::atomic::plugin_error_from_d1(err);
        assert_eq!(
            mapped.code,
            bookclerk_plugin_sdk::PluginErrorCode::Conflict,
            "{mapped}"
        );
    }

    #[tokio::test]
    async fn executing_mock_cancel_before_begin() {
        let (_server, proxy, _conn, _interrupt, _drop, _oversize) = executing_proxy().await;
        bookclerk_library::inject_atomic_interrupt(
            bookclerk_library::AtomicInterruptPhase::BeforeBegin,
            bookclerk_library::AtomicInterruptKind::Cancel,
        );
        let req = typed_sql_req(
            "c",
            vec![(
                "SELECT 1",
                bookclerk_plugin_sdk::DbPlanStatementKind::Select,
            )],
            0,
        );
        let err = run_named_atomic(&proxy, req).await.unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");
    }

    #[tokio::test]
    async fn executing_mock_interrupt_at_http_return_is_ambiguous() {
        let (_server, proxy, _conn, _interrupt, _drop, _oversize) = executing_proxy().await;
        bookclerk_library::inject_atomic_interrupt(
            bookclerk_library::AtomicInterruptPhase::AroundCommit,
            bookclerk_library::AtomicInterruptKind::Cancel,
        );
        let req = typed_sql_req(
            "c2",
            vec![(
                "SELECT 1",
                bookclerk_plugin_sdk::DbPlanStatementKind::Select,
            )],
            0,
        );
        let err = run_named_atomic(&proxy, req).await.unwrap_err();
        assert!(crate::atomic::is_ambiguous_d1(&err), "{err}");
    }

    #[tokio::test]
    async fn executing_mock_http_503_is_ambiguous() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/query$"))
            .respond_with(ResponseTemplate::new(503).set_body_json(json!({
                "success": false,
                "errors": [{"message": "unavailable"}]
            })))
            .mount(&server)
            .await;
        let proxy = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        let req = typed_sql_req(
            "t",
            vec![(
                "SELECT 1",
                bookclerk_plugin_sdk::DbPlanStatementKind::Select,
            )],
            0,
        );
        let err = run_named_atomic(&proxy, req).await.unwrap_err();
        let mapped = crate::atomic::plugin_error_from_d1(err);
        assert_eq!(
            mapped.code,
            bookclerk_plugin_sdk::PluginErrorCode::Unavailable,
            "{mapped}"
        );
    }

    #[tokio::test]
    async fn executing_mock_row_cap_fails_closed() {
        let (_server, proxy, conn, _interrupt, _drop, _oversize) = executing_proxy().await;
        let cap = bookclerk_plugin_abi::DbCapabilities::advertised_d1().max_result_rows as usize;
        {
            let db = conn.lock().expect("sqlite mutex");
            db.execute_batch("CREATE TABLE rowcap (x INTEGER)").unwrap();
            for i in 0..=cap {
                db.execute("INSERT INTO rowcap (x) VALUES (?1)", [i as i64])
                    .unwrap();
            }
        }
        let req = typed_sql_req(
            "row-cap",
            vec![(
                "SELECT x FROM rowcap",
                bookclerk_plugin_sdk::DbPlanStatementKind::Select,
            )],
            0,
        );
        let err = run_named_atomic(&proxy, req).await.unwrap_err();
        assert!(
            err.to_string().contains("maxResultRows"),
            "row cap must fail closed: {err}"
        );
        assert!(
            crate::atomic::is_ambiguous_d1(&err),
            "over-cap rows after HTTP commit must be ambiguous: {err}"
        );
    }

    #[tokio::test]
    async fn executing_mock_host_schema_and_replay() {
        let (_server, proxy, _conn, _interrupt, _drop, _oversize) = executing_proxy().await;
        let db = sea_orm::Database::connect_proxy(
            DatabaseBackend::Sqlite,
            std::sync::Arc::new(Box::new(proxy.clone())),
        )
        .await
        .unwrap();
        let proxy_for_batch = proxy.clone();
        bookclerk_library::apply_host_schema_with_batch(
            &db,
            bookclerk_library::HostSchemaKind::AtomicBatchMarker,
            move |stmts| {
                let proxy = proxy_for_batch.clone();
                async move { run_schema_batch(proxy, stmts).await }
            },
        )
        .await
        .expect("host D1 schema");
        let now = "2024-06-01T00:00:00Z";
        let compiled = bookclerk_library::compile_named_request(
            "d1-enq",
            &bookclerk_library::DbAtomicParams::EnqueueJob {
                kind: "scan".into(),
                payload_json: r#"{"v":1,"account":"a"}"#.into(),
                priority: 0,
                max_attempts: 3,
                max_pending: 10,
                run_after: None,
            },
            now,
        )
        .unwrap();
        let req = compiled.clone().into_typed_request("d1-enq");
        let first = run_named_atomic(&proxy, req).await.expect("first atomic");
        let interpreted =
            bookclerk_library::interpret_typed_exec(&compiled, &first, &compiled.expected_hash);
        assert_eq!(interpreted.status, bookclerk_library::atomic_status::OK);
        let replay_req = compiled.clone().into_typed_request("d1-enq");
        let replay = run_named_atomic(&proxy, replay_req).await.expect("replay");
        let replayed =
            bookclerk_library::interpret_typed_exec(&compiled, &replay, &compiled.expected_hash);
        assert!(replayed.replayed, "same operationId must replay");
    }

    async fn run_schema_batch(proxy: D1Proxy, stmts: Vec<String>) -> bookclerk_library::Result<()> {
        // Adapter edge: split the canonical host-schema pack for the SQLite
        // family before the HTTP batch (mirrors `run_typed_atomic`).
        let stmts = bookclerk_db_exec::expand_host_schema_batch(DatabaseBackend::Sqlite, &stmts)
            .unwrap_or(stmts);
        let batch: Vec<(String, Vec<JsonValue>)> = stmts
            .into_iter()
            .map(|sql| {
                (
                    bookclerk_db_exec::lower_canonical_sql(DatabaseBackend::Sqlite, &sql),
                    Vec::new(),
                )
            })
            .collect();
        let raw = proxy
            .run_batch(&batch)
            .await
            .map_err(|err| bookclerk_library::LibraryError::Orm(sea_orm::DbErr::from(err)))?;
        if let Some(results) = raw.get("result").and_then(JsonValue::as_array) {
            for entry in results {
                if entry.get("success").and_then(JsonValue::as_bool) == Some(false) {
                    let msg = entry
                        .get("error")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("D1 batch statement failed");
                    return Err(bookclerk_library::LibraryError::Orm(
                        sea_orm::DbErr::Custom(msg.into()),
                    ));
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn executing_mock_schema_crash_retries() {
        let (_server, proxy, _conn, interrupt, _drop, _oversize) = executing_proxy().await;
        let db = sea_orm::Database::connect_proxy(
            DatabaseBackend::Sqlite,
            std::sync::Arc::new(Box::new(proxy.clone())),
        )
        .await
        .unwrap();
        interrupt.store(2, std::sync::atomic::Ordering::SeqCst);
        let proxy_for_batch = proxy.clone();
        let err = bookclerk_library::apply_host_schema_with_batch(
            &db,
            bookclerk_library::HostSchemaKind::AtomicBatchMarker,
            move |stmts| {
                let proxy = proxy_for_batch.clone();
                async move { run_schema_batch(proxy, stmts).await }
            },
        )
        .await
        .expect_err("interrupt mid-version");
        assert!(
            err.to_string().to_lowercase().contains("cancel") || err.to_string().contains("failed"),
            "{err}"
        );
        let proxy_for_batch = proxy.clone();
        bookclerk_library::apply_host_schema_with_batch(
            &db,
            bookclerk_library::HostSchemaKind::AtomicBatchMarker,
            move |stmts| {
                let proxy = proxy_for_batch.clone();
                async move { run_schema_batch(proxy, stmts).await }
            },
        )
        .await
        .expect("retry after crash");
    }

    #[tokio::test]
    async fn executing_mock_typed_shared_vectors() {
        let (_server, proxy, _conn, _interrupt, _drop, _oversize) = executing_proxy().await;
        let db = sea_orm::Database::connect_proxy(
            DatabaseBackend::Sqlite,
            std::sync::Arc::new(Box::new(proxy.clone())),
        )
        .await
        .unwrap();
        let proxy_for_batch = proxy.clone();
        bookclerk_library::apply_host_schema_with_batch(
            &db,
            bookclerk_library::HostSchemaKind::AtomicBatchMarker,
            move |stmts| {
                let proxy = proxy_for_batch.clone();
                async move { run_schema_batch(proxy, stmts).await }
            },
        )
        .await
        .expect("host D1 schema");
        bookclerk_library::sql_plan::run_typed_request_vectors(
            bookclerk_plugin_abi::DbCapabilities::advertised_d1(),
            bookclerk_plugin_abi::DbCapabilities::advertised_d1().max_result_rows,
            |req| {
                let proxy = proxy.clone();
                async move {
                    proxy
                        .run_typed_atomic(
                            &req,
                            bookclerk_plugin_abi::GuestReceiptPersist::default(),
                            &[],
                        )
                        .await
                }
            },
        )
        .await;
    }

    #[tokio::test]
    async fn executing_mock_i64_http_boundary_roundtrip() {
        use bookclerk_plugin_sdk::{
            DbPlanStatementKind, DbResultSelection, DbValue, ExecuteRequest, TypedDbStatement,
        };

        const JS_MAX_SAFE: i64 = 9_007_199_254_740_991;
        const JS_UNSAFE: i64 = 9_007_199_254_740_992;

        let (_server, proxy, _conn, _interrupt, _drop, _oversize) = executing_proxy().await;
        for (label, value) in [
            ("min", i64::MIN),
            ("max", i64::MAX),
            ("js_max_safe", JS_MAX_SAFE),
            ("js_unsafe", JS_UNSAFE),
        ] {
            let req = ExecuteRequest {
                operation_id: format!("d1-i64-{label}"),
                request_hash: String::new(),
                statements: vec![TypedDbStatement {
                    sql: "SELECT ? AS v".into(),
                    parameters: vec![DbValue::Int64(value)],
                    kind: DbPlanStatementKind::Select,
                    max_rows: 0,
                    result_selection: DbResultSelection::Rows,
                }],
                deadline_unix_ms: 0,
            };
            let reply = proxy
                .run_typed_atomic(
                    &req,
                    bookclerk_plugin_abi::GuestReceiptPersist::default(),
                    &[],
                )
                .await
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            let got = reply.statements[0].rows[0].values[0].clone();
            assert_eq!(got, DbValue::Int64(value), "{label}");
        }
    }

    #[tokio::test]
    async fn executing_mock_deadline_includes_http_mutex_wait() {
        let (_server, proxy, _conn, _interrupt, _drop, _oversize) = executing_proxy().await;
        let _hold = proxy.lock_http_for_test().await;
        let deadline = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
            .saturating_add(80);
        let req = typed_sql_req(
            "op-dl",
            vec![(
                "SELECT 1 AS n",
                bookclerk_plugin_sdk::DbPlanStatementKind::Select,
            )],
            deadline,
        );
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            run_named_atomic(&proxy, req),
        )
        .await
        .expect("run_typed_atomic must return when the deadline elapses")
        .expect_err("held HTTP slot must expire the deadline");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("deadline"),
            "mutex wait must count toward deadlineUnixMs: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn executing_mock_concurrent_independent_proxies_apply_schema() {
        let (server, proxy1, _conn, _interrupt, _drop, _oversize) = executing_proxy().await;
        let proxy2 = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        let db1 = sea_orm::Database::connect_proxy(
            DatabaseBackend::Sqlite,
            std::sync::Arc::new(Box::new(proxy1.clone())),
        )
        .await
        .unwrap();
        let db2 = sea_orm::Database::connect_proxy(
            DatabaseBackend::Sqlite,
            std::sync::Arc::new(Box::new(proxy2.clone())),
        )
        .await
        .unwrap();
        let p1 = proxy1.clone();
        let p2 = proxy2.clone();
        let (a, b) = tokio::join!(
            bookclerk_library::apply_host_schema_with_batch(
                &db1,
                bookclerk_library::HostSchemaKind::AtomicBatchMarker,
                move |stmts| {
                    let proxy = p1.clone();
                    async move { run_schema_batch(proxy, stmts).await }
                },
            ),
            bookclerk_library::apply_host_schema_with_batch(
                &db2,
                bookclerk_library::HostSchemaKind::AtomicBatchMarker,
                move |stmts| {
                    let proxy = p2.clone();
                    async move { run_schema_batch(proxy, stmts).await }
                },
            ),
        );
        a.expect("independent proxy 1 schema");
        b.expect("independent proxy 2 schema");
    }

    fn select_one_req(op: &str) -> bookclerk_plugin_abi::ExecuteRequest {
        typed_sql_req(
            op,
            vec![(
                "SELECT 1 AS n",
                bookclerk_plugin_sdk::DbPlanStatementKind::Select,
            )],
            0,
        )
    }

    async fn oversized_after_commit(status: u16) -> sea_orm::DbErr {
        let (_server, proxy, _conn, _interrupt, _drop, oversized) = executing_proxy().await;
        oversized.store(status, std::sync::atomic::Ordering::SeqCst);
        run_named_atomic(&proxy, select_one_req(&format!("big-{status}")))
            .await
            .expect_err("oversized post-commit body must fail")
    }

    #[tokio::test]
    async fn executing_mock_oversized_2xx_body_after_commit_is_ambiguous() {
        let err = oversized_after_commit(200).await;
        assert!(crate::atomic::is_ambiguous_d1(&err), "{err}");
        let mapped = crate::atomic::plugin_error_from_d1(err);
        assert_eq!(
            mapped.code,
            bookclerk_plugin_sdk::PluginErrorCode::Unavailable,
            "{mapped}"
        );
    }

    #[tokio::test]
    async fn executing_mock_oversized_429_body_after_commit_is_ambiguous() {
        let err = oversized_after_commit(429).await;
        assert!(crate::atomic::is_ambiguous_d1(&err), "{err}");
        let mapped = crate::atomic::plugin_error_from_d1(err);
        assert_eq!(
            mapped.code,
            bookclerk_plugin_sdk::PluginErrorCode::Unavailable,
            "{mapped}"
        );
    }

    #[tokio::test]
    async fn executing_mock_oversized_400_body_after_commit_is_permanent() {
        let err = oversized_after_commit(400).await;
        assert!(!crate::atomic::is_ambiguous_d1(&err), "{err}");
        let mapped = crate::atomic::plugin_error_from_d1(err);
        assert_eq!(
            mapped.code,
            bookclerk_plugin_sdk::PluginErrorCode::InvalidParams,
            "{mapped}"
        );
    }

    #[tokio::test]
    async fn executing_mock_committed_reply_lost_still_completes() {
        let (_server, proxy, _conn, _interrupt, drop_reply, _oversize) = executing_proxy().await;
        drop_reply.store(true, std::sync::atomic::Ordering::SeqCst);
        let db = sea_orm::Database::connect_proxy(
            DatabaseBackend::Sqlite,
            std::sync::Arc::new(Box::new(proxy.clone())),
        )
        .await
        .unwrap();
        let proxy_for_batch = proxy.clone();
        bookclerk_library::apply_host_schema_with_batch(
            &db,
            bookclerk_library::HostSchemaKind::AtomicBatchMarker,
            move |stmts| {
                let proxy = proxy_for_batch.clone();
                async move { run_schema_batch(proxy, stmts).await }
            },
        )
        .await
        .expect("reload after committed-but-lost HTTP reply");
    }

    struct ProxyTypedExec {
        proxy: D1Proxy,
    }

    #[async_trait::async_trait]
    impl bookclerk_library::TypedAtomicExec for ProxyTypedExec {
        async fn execute_typed(
            &self,
            envelope: bookclerk_plugin_abi::HostExecuteEnvelope,
        ) -> std::result::Result<
            bookclerk_plugin_sdk::ExecuteReply,
            bookclerk_plugin_sdk::PluginError,
        > {
            self.proxy
                .run_typed_atomic(&envelope.request, envelope.guest_receipt, &envelope.proofs)
                .await
                .map_err(crate::atomic::plugin_error_from_d1)
        }
    }

    /// Guest `executeAtomic` over the D1 HTTP batch path must replay the stored
    /// caller-visible result (not a gated no-op `rowsAffected = 0`).
    #[tokio::test]
    async fn executing_mock_guest_typed_replay_preserves_rows_affected() {
        use bookclerk_library::GuestSqlPolicy;
        use bookclerk_plugin_sdk::{
            DbPlanStatementKind, DbResultSelection, ExecuteRequest, TypedDbStatement,
        };

        let (_server, proxy, _conn, _interrupt, _drop, _oversize) = executing_proxy().await;
        let db = sea_orm::Database::connect_proxy(
            DatabaseBackend::Sqlite,
            std::sync::Arc::new(Box::new(proxy.clone())),
        )
        .await
        .unwrap();
        let proxy_for_batch = proxy.clone();
        bookclerk_library::apply_host_schema_with_batch(
            &db,
            bookclerk_library::HostSchemaKind::AtomicBatchMarker,
            move |stmts| {
                let proxy = proxy_for_batch.clone();
                async move { run_schema_batch(proxy, stmts).await }
            },
        )
        .await
        .expect("host schema for guest typed replay");

        let store = bookclerk_library::LibraryStore::from_connection(db)
            .with_db_capabilities(bookclerk_plugin_abi::DbCapabilities::advertised_d1())
            .with_typed_exec(std::sync::Arc::new(ProxyTypedExec {
                proxy: proxy.clone(),
            }));
        let policy = GuestSqlPolicy::allow_tables(["db_serialization_slots", "db_atomic_receipts"]);
        let guest_hash = String::new();
        let req = ExecuteRequest {
            operation_id: "d1-guest-replay".into(),
            request_hash: guest_hash,
            statements: vec![TypedDbStatement {
                sql: "INSERT INTO db_serialization_slots (slot_key, bump) VALUES ('d1-guest', 1)"
                    .into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        };
        let first = store
            .execute_guest_atomic(req.clone(), &policy)
            .await
            .expect("first guest typed batch");
        assert_eq!(first.statements[0].rows_affected, 1);

        let replay = store
            .execute_guest_atomic(req, &policy)
            .await
            .expect("replay guest typed batch");
        assert_eq!(
            replay.statements[0].rows_affected, 1,
            "replay must return the stored caller-visible rowsAffected, not gated 0"
        );
    }

    /// After commit with a lost HTTP reply, a retry must not fabricate a no-op
    /// result; the caller receives `unavailable` and the mutation applies once.
    #[tokio::test]
    async fn executing_mock_guest_typed_committed_reply_lost_is_unavailable() {
        use bookclerk_library::GuestSqlPolicy;
        use bookclerk_plugin_sdk::{
            DbPlanStatementKind, DbResultSelection, ExecuteRequest, PluginErrorCode,
            TypedDbStatement,
        };

        let (_server, proxy, conn, _interrupt, drop_reply, _oversize) = executing_proxy().await;
        let db = sea_orm::Database::connect_proxy(
            DatabaseBackend::Sqlite,
            std::sync::Arc::new(Box::new(proxy.clone())),
        )
        .await
        .unwrap();
        let proxy_for_batch = proxy.clone();
        bookclerk_library::apply_host_schema_with_batch(
            &db,
            bookclerk_library::HostSchemaKind::AtomicBatchMarker,
            move |stmts| {
                let proxy = proxy_for_batch.clone();
                async move { run_schema_batch(proxy, stmts).await }
            },
        )
        .await
        .expect("host schema for lost-reply guest typed");

        let store = bookclerk_library::LibraryStore::from_connection(db)
            .with_db_capabilities(bookclerk_plugin_abi::DbCapabilities::advertised_d1())
            .with_typed_exec(std::sync::Arc::new(ProxyTypedExec {
                proxy: proxy.clone(),
            }));
        let policy = GuestSqlPolicy::allow_tables(["db_serialization_slots", "db_atomic_receipts"]);
        drop_reply.store(true, std::sync::atomic::Ordering::SeqCst);
        let req = ExecuteRequest {
            operation_id: "d1-guest-lost-reply".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "INSERT INTO db_serialization_slots (slot_key, bump) VALUES ('lost-reply', 1)"
                    .into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        };
        let err = store
            .execute_guest_atomic(req, &policy)
            .await
            .expect_err("lost reply before finalize must be unavailable");
        assert_eq!(err.code, PluginErrorCode::Unavailable, "{err}");
        let count: i64 = conn
            .lock()
            .expect("sqlite")
            .query_row(
                "SELECT COUNT(*) FROM db_serialization_slots WHERE slot_key = 'lost-reply'",
                [],
                |row| row.get(0),
            )
            .expect("count slot rows");
        assert_eq!(count, 1, "mutation must commit exactly once");
    }

    /// After the mutating batch commits, a failed `pragma_table_info` fetch
    /// is `unavailable` (not Internal). Retry follows receipt semantics.
    #[tokio::test]
    async fn executing_mock_post_commit_pragma_failure_is_unavailable() {
        use bookclerk_library::GuestSqlPolicy;
        use bookclerk_plugin_sdk::{
            DbPlanStatementKind, DbResultSelection, ExecuteRequest, PluginErrorCode,
            TypedDbStatement,
        };

        let (_server, proxy, conn, _interrupt, _drop, _oversize, fail_pragma) =
            executing_proxy_inner().await;
        let db = sea_orm::Database::connect_proxy(
            DatabaseBackend::Sqlite,
            std::sync::Arc::new(Box::new(proxy.clone())),
        )
        .await
        .unwrap();
        let proxy_for_batch = proxy.clone();
        bookclerk_library::apply_host_schema_with_batch(
            &db,
            bookclerk_library::HostSchemaKind::AtomicBatchMarker,
            move |stmts| {
                let proxy = proxy_for_batch.clone();
                async move { run_schema_batch(proxy, stmts).await }
            },
        )
        .await
        .expect("host schema for pragma-fail guest typed");

        let store = bookclerk_library::LibraryStore::from_connection(db)
            .with_db_capabilities(bookclerk_plugin_abi::DbCapabilities::advertised_d1())
            .with_typed_exec(std::sync::Arc::new(ProxyTypedExec {
                proxy: proxy.clone(),
            }));
        let policy = GuestSqlPolicy::allow_tables(["db_serialization_slots", "db_atomic_receipts"]);
        fail_pragma.store(true, std::sync::atomic::Ordering::SeqCst);
        let req = ExecuteRequest {
            operation_id: "d1-guest-pragma-fail".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql:
                    "INSERT INTO db_serialization_slots (slot_key, bump) VALUES ('pragma-fail', 1)"
                        .into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        };
        let err = store
            .execute_guest_atomic(req.clone(), &policy)
            .await
            .expect_err("post-commit pragma failure must be unavailable");
        assert_eq!(err.code, PluginErrorCode::Unavailable, "{err}");
        let count: i64 = conn
            .lock()
            .expect("sqlite")
            .query_row(
                "SELECT COUNT(*) FROM db_serialization_slots WHERE slot_key = 'pragma-fail'",
                [],
                |row| row.get(0),
            )
            .expect("count slot rows");
        assert_eq!(count, 1, "mutation must commit exactly once");
        fail_pragma.store(false, std::sync::atomic::Ordering::SeqCst);
        let retry = store.execute_guest_atomic(req, &policy).await;
        match retry {
            Ok(replay) => assert_eq!(replay.statements.len(), 1),
            Err(err) => assert_eq!(
                err.code,
                PluginErrorCode::Unavailable,
                "receipt-loss retry stays unavailable: {err}"
            ),
        }
        let count: i64 = conn
            .lock()
            .expect("sqlite")
            .query_row(
                "SELECT COUNT(*) FROM db_serialization_slots WHERE slot_key = 'pragma-fail'",
                [],
                |row| row.get(0),
            )
            .expect("count after retry");
        assert_eq!(count, 1, "retry must not insert a second row");
    }

    /// Two hosts claiming the same operationId with different hashes must not
    /// both apply ungated guest DDL. The claim stub INSERT is the mutex.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn executing_mock_concurrent_claim_only_one_schema_change() {
        use bookclerk_library::GuestSqlPolicy;
        use bookclerk_plugin_sdk::{
            DbPlanStatementKind, DbResultSelection, ExecuteRequest, PluginErrorCode,
            TypedDbStatement,
        };

        let (server, proxy1, conn, _interrupt, _drop, _oversize, _pragma) =
            executing_proxy_inner().await;
        let proxy2 = D1Proxy::new(server.uri(), "acct".into(), "dbid".into(), "token".into());
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        proxy1.pause_claims(std::sync::Arc::clone(&barrier));
        proxy2.pause_claims(barrier);
        let db = sea_orm::Database::connect_proxy(
            DatabaseBackend::Sqlite,
            std::sync::Arc::new(Box::new(proxy1.clone())),
        )
        .await
        .unwrap();
        let proxy_for_batch = proxy1.clone();
        bookclerk_library::apply_host_schema_with_batch(
            &db,
            bookclerk_library::HostSchemaKind::AtomicBatchMarker,
            move |stmts| {
                let proxy = proxy_for_batch.clone();
                async move { run_schema_batch(proxy, stmts).await }
            },
        )
        .await
        .expect("host schema for concurrent claim");

        let caps = bookclerk_plugin_abi::DbCapabilities::advertised_d1();
        let policy = GuestSqlPolicy::binding_owned();
        let req_alpha = ExecuteRequest {
            operation_id: "d1-claim-race".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "CREATE TABLE IF NOT EXISTS alpha (id INTEGER PRIMARY KEY)".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::AffectedRows,
            }],
            deadline_unix_ms: 0,
        };
        let req_beta = ExecuteRequest {
            operation_id: "d1-claim-race".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "CREATE TABLE IF NOT EXISTS beta (id INTEGER PRIMARY KEY)".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::AffectedRows,
            }],
            deadline_unix_ms: 0,
        };
        let p1 = proxy1.clone();
        let p2 = proxy2.clone();
        let (a, b) = tokio::join!(
            bookclerk_library::execute_guest_atomic_with(req_alpha, &caps, &policy, |envelope| {
                let proxy = p1.clone();
                async move {
                    proxy
                        .run_typed_atomic(
                            &envelope.request,
                            envelope.guest_receipt,
                            &envelope.proofs,
                        )
                        .await
                        .map_err(crate::atomic::plugin_error_from_d1)
                }
            }),
            bookclerk_library::execute_guest_atomic_with(req_beta, &caps, &policy, |envelope| {
                let proxy = p2.clone();
                async move {
                    proxy
                        .run_typed_atomic(
                            &envelope.request,
                            envelope.guest_receipt,
                            &envelope.proofs,
                        )
                        .await
                        .map_err(crate::atomic::plugin_error_from_d1)
                }
            }),
        );
        let outcomes = [a, b];
        let wins = outcomes.iter().filter(|r| r.is_ok()).count();
        let conflicts = outcomes
            .iter()
            .filter(|r| {
                r.as_ref()
                    .err()
                    .is_some_and(|e| e.code == PluginErrorCode::Conflict)
            })
            .count();
        assert_eq!(wins, 1, "exactly one claimer applies DDL: {outcomes:?}");
        assert_eq!(
            conflicts, 1,
            "loser must be conflict (hash mismatch): {outcomes:?}"
        );
        let alpha: i64 = conn
            .lock()
            .expect("sqlite")
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'alpha'",
                [],
                |row| row.get(0),
            )
            .expect("alpha");
        let beta: i64 = conn
            .lock()
            .expect("sqlite")
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'beta'",
                [],
                |row| row.get(0),
            )
            .expect("beta");
        assert_eq!(
            alpha + beta,
            1,
            "exactly one guest table must exist (alpha={alpha} beta={beta})"
        );
    }

    /// A crash after the claim stub INSERT (status=`claimed`) and before ungated
    /// DDL must resume the same hash on retry instead of treating the empty
    /// payload as result-lost.
    #[tokio::test]
    async fn executing_mock_claimed_ddl_resumes_after_lost_claim() {
        use bookclerk_library::GuestSqlPolicy;
        use bookclerk_plugin_sdk::{
            DbPlanStatementKind, DbResultSelection, ExecuteRequest, PluginErrorCode,
            TypedDbStatement,
        };

        let (_server, proxy, conn, _interrupt, _drop, _oversize) = executing_proxy().await;
        let db = sea_orm::Database::connect_proxy(
            DatabaseBackend::Sqlite,
            std::sync::Arc::new(Box::new(proxy.clone())),
        )
        .await
        .unwrap();
        let proxy_for_batch = proxy.clone();
        bookclerk_library::apply_host_schema_with_batch(
            &db,
            bookclerk_library::HostSchemaKind::AtomicBatchMarker,
            move |stmts| {
                let proxy = proxy_for_batch.clone();
                async move { run_schema_batch(proxy, stmts).await }
            },
        )
        .await
        .expect("host schema for claimed resume");

        let caps = bookclerk_plugin_abi::DbCapabilities::advertised_d1();
        let policy = GuestSqlPolicy::binding_owned();
        let req = ExecuteRequest {
            operation_id: "d1-claim-resume".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "CREATE TABLE IF NOT EXISTS resume_notes (id INTEGER PRIMARY KEY)".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::AffectedRows,
            }],
            deadline_unix_ms: 0,
        };
        proxy.fail_next_won_claim();
        let first =
            bookclerk_library::execute_guest_atomic_with(req.clone(), &caps, &policy, |envelope| {
                let proxy = proxy.clone();
                async move {
                    proxy
                        .run_typed_atomic(
                            &envelope.request,
                            envelope.guest_receipt,
                            &envelope.proofs,
                        )
                        .await
                        .map_err(crate::atomic::plugin_error_from_d1)
                }
            })
            .await;
        let err = first.expect_err("injected crash after claim");
        assert_eq!(err.code, PluginErrorCode::Unavailable, "{err}");
        let tables_after_crash: i64 = conn
            .lock()
            .expect("sqlite")
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'resume_notes'",
                [],
                |row| row.get(0),
            )
            .expect("count after crash");
        assert_eq!(
            tables_after_crash, 0,
            "DDL must not have run before the crash"
        );

        let retry = bookclerk_library::execute_guest_atomic_with(req, &caps, &policy, |envelope| {
            let proxy = proxy.clone();
            async move {
                proxy
                    .run_typed_atomic(&envelope.request, envelope.guest_receipt, &envelope.proofs)
                    .await
                    .map_err(crate::atomic::plugin_error_from_d1)
            }
        })
        .await
        .expect("same-hash retry must resume claimed DDL");
        assert_eq!(retry.statements.len(), 1);
        let tables_after_retry: i64 = conn
            .lock()
            .expect("sqlite")
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'resume_notes'",
                [],
                |row| row.get(0),
            )
            .expect("count after retry");
        assert_eq!(tables_after_retry, 1, "retry must apply the claimed DDL");
    }

    /// Mixed DDL+DML on executing D1 must apply the INSERT on first execution
    /// (claimed-owner ungates DML) and must not double-insert on same-token replay.
    #[tokio::test]
    async fn executing_mock_mixed_ddl_dml_applies_insert_once() {
        use bookclerk_library::GuestSqlPolicy;
        use bookclerk_plugin_sdk::{
            DbPlanStatementKind, DbResultSelection, ExecuteRequest, TypedDbStatement,
        };

        let (_server, proxy, conn, _interrupt, _drop, _oversize) = executing_proxy().await;
        let db = sea_orm::Database::connect_proxy(
            DatabaseBackend::Sqlite,
            std::sync::Arc::new(Box::new(proxy.clone())),
        )
        .await
        .unwrap();
        let proxy_for_batch = proxy.clone();
        bookclerk_library::apply_host_schema_with_batch(
            &db,
            bookclerk_library::HostSchemaKind::AtomicBatchMarker,
            move |stmts| {
                let proxy = proxy_for_batch.clone();
                async move { run_schema_batch(proxy, stmts).await }
            },
        )
        .await
        .expect("host schema for mixed batch");

        let caps = bookclerk_plugin_abi::DbCapabilities::advertised_d1();
        let policy = GuestSqlPolicy::binding_owned();
        let mixed = || ExecuteRequest {
            operation_id: "d1-mixed-once".into(),
            request_hash: String::new(),
            statements: vec![
                TypedDbStatement {
                    sql: "CREATE TABLE IF NOT EXISTS counters (id INTEGER PRIMARY KEY, n INTEGER)"
                        .into(),
                    parameters: vec![],
                    kind: DbPlanStatementKind::Execute,
                    max_rows: 0,
                    result_selection: DbResultSelection::Discard,
                },
                TypedDbStatement {
                    sql: "INSERT INTO counters (id, n) VALUES (?, ?)".into(),
                    parameters: vec![
                        bookclerk_plugin_abi::DbValue::Int64(1),
                        bookclerk_plugin_abi::DbValue::Int64(1),
                    ],
                    kind: DbPlanStatementKind::Execute,
                    max_rows: 0,
                    result_selection: DbResultSelection::AffectedRows,
                },
            ],
            deadline_unix_ms: 0,
        };
        let first =
            bookclerk_library::execute_guest_atomic_with(mixed(), &caps, &policy, |envelope| {
                let proxy = proxy.clone();
                async move {
                    proxy
                        .run_typed_atomic(
                            &envelope.request,
                            envelope.guest_receipt,
                            &envelope.proofs,
                        )
                        .await
                        .map_err(crate::atomic::plugin_error_from_d1)
                }
            })
            .await
            .expect("first mixed");
        assert_eq!(first.statements[1].rows_affected, 1);
        let replay =
            bookclerk_library::execute_guest_atomic_with(mixed(), &caps, &policy, |envelope| {
                let proxy = proxy.clone();
                async move {
                    proxy
                        .run_typed_atomic(
                            &envelope.request,
                            envelope.guest_receipt,
                            &envelope.proofs,
                        )
                        .await
                        .map_err(crate::atomic::plugin_error_from_d1)
                }
            })
            .await
            .expect("replay mixed");
        assert_eq!(replay.statements[1].rows_affected, 1);
        let count: i64 = conn
            .lock()
            .expect("sqlite")
            .query_row("SELECT COUNT(*) FROM counters", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 1, "D1 mixed batch must not double-insert");
    }

    /// Mixed DDL+DML whose INSERT stores the host write-gate text in a string
    /// and a comment must keep that payload intact after claimed-owner ungating.
    #[tokio::test]
    async fn executing_mock_mixed_ddl_dml_preserves_gate_text_in_literal_and_comment() {
        use bookclerk_library::GuestSqlPolicy;
        use bookclerk_plugin_sdk::{
            DbPlanStatementKind, DbResultSelection, ExecuteRequest, TypedDbStatement,
        };

        let (_server, proxy, conn, _interrupt, _drop, _oversize) = executing_proxy().await;
        let db = sea_orm::Database::connect_proxy(
            DatabaseBackend::Sqlite,
            std::sync::Arc::new(Box::new(proxy.clone())),
        )
        .await
        .unwrap();
        let proxy_for_batch = proxy.clone();
        bookclerk_library::apply_host_schema_with_batch(
            &db,
            bookclerk_library::HostSchemaKind::AtomicBatchMarker,
            move |stmts| {
                let proxy = proxy_for_batch.clone();
                async move { run_schema_batch(proxy, stmts).await }
            },
        )
        .await
        .expect("host schema for mixed gate batch");

        let caps = bookclerk_plugin_abi::DbCapabilities::advertised_d1();
        let policy = GuestSqlPolicy::binding_owned();
        let mixed = || ExecuteRequest {
            operation_id: "d1-mixed-gate-lit".into(),
            request_hash: String::new(),
            statements: vec![
                TypedDbStatement {
                    sql: bookclerk_db_exec::sql_v1::MIXED_GATE_LITERAL_DDL.into(),
                    parameters: vec![],
                    kind: DbPlanStatementKind::Execute,
                    max_rows: 0,
                    result_selection: DbResultSelection::Discard,
                },
                TypedDbStatement {
                    sql: bookclerk_db_exec::sql_v1::mixed_gate_literal_insert(),
                    parameters: vec![],
                    kind: DbPlanStatementKind::Execute,
                    max_rows: 0,
                    result_selection: DbResultSelection::AffectedRows,
                },
            ],
            deadline_unix_ms: 0,
        };
        let first =
            bookclerk_library::execute_guest_atomic_with(mixed(), &caps, &policy, |envelope| {
                let proxy = proxy.clone();
                async move {
                    proxy
                        .run_typed_atomic(
                            &envelope.request,
                            envelope.guest_receipt,
                            &envelope.proofs,
                        )
                        .await
                        .map_err(crate::atomic::plugin_error_from_d1)
                }
            })
            .await
            .expect("first mixed gate");
        assert_eq!(first.statements[1].rows_affected, 1);
        let replay =
            bookclerk_library::execute_guest_atomic_with(mixed(), &caps, &policy, |envelope| {
                let proxy = proxy.clone();
                async move {
                    proxy
                        .run_typed_atomic(
                            &envelope.request,
                            envelope.guest_receipt,
                            &envelope.proofs,
                        )
                        .await
                        .map_err(crate::atomic::plugin_error_from_d1)
                }
            })
            .await
            .expect("replay mixed gate");
        assert_eq!(replay.statements[1].rows_affected, 1);
        let body: String = conn
            .lock()
            .expect("sqlite")
            .query_row("SELECT body FROM gated_notes WHERE id = 1", [], |row| {
                row.get(0)
            })
            .expect("body");
        assert_eq!(body, bookclerk_db_exec::GUEST_RECEIPT_WRITE_GATE);
        let count: i64 = conn
            .lock()
            .expect("sqlite")
            .query_row("SELECT COUNT(*) FROM gated_notes", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 1, "D1 mixed gate batch must not double-insert");
    }

    /// Portable SQL v1 helpers and AUTOINCREMENT/BLOB DDL execute on D1.
    #[tokio::test]
    async fn executing_mock_portable_functions_and_binding_ddl() {
        use bookclerk_plugin_sdk::{
            DbPlanStatementKind, DbResultSelection, ExecuteRequest, TypedDbStatement,
        };

        let (_server, proxy, conn, _interrupt, _drop, _oversize) = executing_proxy().await;
        let db = sea_orm::Database::connect_proxy(
            DatabaseBackend::Sqlite,
            std::sync::Arc::new(Box::new(proxy.clone())),
        )
        .await
        .unwrap();
        let proxy_for_batch = proxy.clone();
        bookclerk_library::apply_host_schema_with_batch(
            &db,
            bookclerk_library::HostSchemaKind::AtomicBatchMarker,
            move |stmts| {
                let proxy = proxy_for_batch.clone();
                async move { run_schema_batch(proxy, stmts).await }
            },
        )
        .await
        .expect("host schema for portable fns");

        let run = |req: ExecuteRequest| {
            let proxy = proxy.clone();
            let db = db.clone();
            async move { execute_d1_guest_atomic(&db, proxy, req).await }
        };
        run(ExecuteRequest {
            operation_id: "d1-ddl-typed".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: bookclerk_db_exec::sql_v1::BINDING_DDL_AUTOINCREMENT_BLOB.into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::Discard,
            }],
            deadline_unix_ms: 0,
        })
        .await
        .expect("typed DDL");
        run(ExecuteRequest {
            operation_id: "d1-ins-typed".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: bookclerk_db_exec::sql_v1::PORTABLE_INSERT.into(),
                parameters: vec![bookclerk_plugin_abi::DbValue::Bytes(
                    bookclerk_db_exec::sql_v1::PORTABLE_INSERT_BLOB.to_vec(),
                )],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::AffectedRows,
            }],
            deadline_unix_ms: 0,
        })
        .await
        .expect("typed insert");
        let reply = run(ExecuteRequest {
            operation_id: "d1-sel-typed".into(),
            request_hash: String::new(),
            statements: vec![
                TypedDbStatement {
                    sql: bookclerk_db_exec::sql_v1::PORTABLE_SELECT.into(),
                    parameters: vec![],
                    kind: DbPlanStatementKind::Select,
                    max_rows: 8,
                    result_selection: DbResultSelection::Rows,
                },
                TypedDbStatement {
                    sql: bookclerk_db_exec::sql_v1::PORTABLE_AGGREGATE_SELECT.into(),
                    parameters: vec![],
                    kind: DbPlanStatementKind::Select,
                    max_rows: 8,
                    result_selection: DbResultSelection::Rows,
                },
            ],
            deadline_unix_ms: 0,
        })
        .await
        .expect("portable select");
        if let Some(err) = bookclerk_db_exec::sql_v1::portable_select_mismatch(&reply.statements[0])
        {
            panic!("{err}");
        }
        if let Some(err) =
            bookclerk_db_exec::sql_v1::portable_aggregate_mismatch(&reply.statements[1])
        {
            panic!("{err}");
        }
        let blob: Vec<u8> = conn
            .lock()
            .expect("sqlite")
            .query_row("SELECT blob FROM typed", [], |row| row.get(0))
            .expect("blob");
        assert_eq!(blob, bookclerk_db_exec::sql_v1::PORTABLE_INSERT_BLOB);
    }

    /// Canonical BOOLEAN columns round-trip as [`DbValue::Boolean`] / typed-null.
    #[tokio::test]
    async fn executing_mock_portable_boolean_column() {
        use bookclerk_plugin_sdk::{
            DbPlanStatementKind, DbResultSelection, ExecuteRequest, TypedDbStatement,
        };

        let (_server, proxy, _conn, _interrupt, _drop, _oversize) = executing_proxy().await;
        let db = sea_orm::Database::connect_proxy(
            DatabaseBackend::Sqlite,
            std::sync::Arc::new(Box::new(proxy.clone())),
        )
        .await
        .unwrap();
        let proxy_for_batch = proxy.clone();
        bookclerk_library::apply_host_schema_with_batch(
            &db,
            bookclerk_library::HostSchemaKind::AtomicBatchMarker,
            move |stmts| {
                let proxy = proxy_for_batch.clone();
                async move { run_schema_batch(proxy, stmts).await }
            },
        )
        .await
        .expect("host schema for boolean");

        let run = |req: ExecuteRequest| {
            let proxy = proxy.clone();
            let db = db.clone();
            async move { execute_d1_guest_atomic(&db, proxy, req).await }
        };
        run(ExecuteRequest {
            operation_id: "d1-ddl-bool".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: bookclerk_db_exec::sql_v1::BINDING_DDL_BOOLEAN.into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::Discard,
            }],
            deadline_unix_ms: 0,
        })
        .await
        .expect("boolean DDL");
        run(ExecuteRequest {
            operation_id: "d1-ins-bool".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: bookclerk_db_exec::sql_v1::PORTABLE_BOOLEAN_INSERT.into(),
                parameters: bookclerk_db_exec::sql_v1::portable_boolean_insert_binds(),
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::AffectedRows,
            }],
            deadline_unix_ms: 0,
        })
        .await
        .expect("boolean insert");
        let reply = run(ExecuteRequest {
            operation_id: "d1-sel-bool".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: bookclerk_db_exec::sql_v1::PORTABLE_BOOLEAN_SELECT.into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Select,
                max_rows: 8,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        })
        .await
        .expect("boolean select");
        if let Some(err) =
            bookclerk_db_exec::sql_v1::portable_boolean_mismatch(&reply.statements[0])
        {
            panic!("{err}");
        }
    }

    /// Lowercase DDL types and `insert or ignore … returning` execute on D1.
    #[tokio::test]
    async fn executing_mock_lowercase_ddl_and_insert_or_ignore_returning() {
        use bookclerk_plugin_sdk::{
            DbPlanStatementKind, DbResultSelection, ExecuteRequest, TypedDbStatement,
        };

        let (_server, proxy, _conn, _interrupt, _drop, _oversize) = executing_proxy().await;
        let db = sea_orm::Database::connect_proxy(
            DatabaseBackend::Sqlite,
            std::sync::Arc::new(Box::new(proxy.clone())),
        )
        .await
        .unwrap();
        let proxy_for_batch = proxy.clone();
        bookclerk_library::apply_host_schema_with_batch(
            &db,
            bookclerk_library::HostSchemaKind::AtomicBatchMarker,
            move |stmts| {
                let proxy = proxy_for_batch.clone();
                async move { run_schema_batch(proxy, stmts).await }
            },
        )
        .await
        .expect("host schema for lowercase");

        let run = |req: ExecuteRequest| {
            let proxy = proxy.clone();
            let db = db.clone();
            async move { execute_d1_guest_atomic(&db, proxy, req).await }
        };
        run(ExecuteRequest {
            operation_id: "d1-ddl-lc".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: bookclerk_db_exec::sql_v1::BINDING_DDL_LOWERCASE.into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::Discard,
            }],
            deadline_unix_ms: 0,
        })
        .await
        .expect("lowercase DDL");
        let inserted = run(ExecuteRequest {
            operation_id: "d1-ins-lc".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: bookclerk_db_exec::sql_v1::PORTABLE_INSERT_OR_IGNORE_RETURNING_LC.into(),
                parameters: bookclerk_db_exec::sql_v1::portable_lowercase_insert_binds(),
                kind: DbPlanStatementKind::Returning,
                max_rows: 1,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        })
        .await
        .expect("lowercase insert returning");
        let bookclerk_plugin_abi::DbValue::Int64(id) = inserted.statements[0].rows[0].values[0]
        else {
            panic!(
                "expected int64 returning id, got {:?}",
                inserted.statements[0].rows[0].values[0]
            );
        };
        assert!(id >= 1, "returning id {id}");
        let reply = run(ExecuteRequest {
            operation_id: "d1-sel-lc".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: bookclerk_db_exec::sql_v1::PORTABLE_LOWERCASE_SELECT.into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Select,
                max_rows: 8,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        })
        .await
        .expect("lowercase select");
        assert_eq!(
            reply.statements[0].rows[0].values[0],
            bookclerk_plugin_abi::DbValue::Bytes(
                bookclerk_db_exec::sql_v1::PORTABLE_INSERT_BLOB.to_vec()
            )
        );
        assert_eq!(
            reply.statements[0].rows[0].values[1],
            bookclerk_plugin_abi::DbValue::Boolean(true)
        );
    }

    /// Unique-only `INSERT OR IGNORE`, NULL-poison min/max, ORDER BY NULLs,
    /// AUTOINCREMENT identity, unquoted fold, and uncast helper wire types.
    #[tokio::test]
    async fn executing_mock_sql_v1_semantic_vectors() {
        use bookclerk_plugin_sdk::{
            DbPlanStatementKind, DbResultSelection, ExecuteRequest, TypedDbStatement,
        };

        let (_server, proxy, _conn, _interrupt, _drop, _oversize) = executing_proxy().await;
        let db = sea_orm::Database::connect_proxy(
            DatabaseBackend::Sqlite,
            std::sync::Arc::new(Box::new(proxy.clone())),
        )
        .await
        .unwrap();
        let proxy_for_batch = proxy.clone();
        bookclerk_library::apply_host_schema_with_batch(
            &db,
            bookclerk_library::HostSchemaKind::AtomicBatchMarker,
            move |stmts| {
                let proxy = proxy_for_batch.clone();
                async move { run_schema_batch(proxy, stmts).await }
            },
        )
        .await
        .expect("host schema for semantic vectors");

        let run = |req: ExecuteRequest| {
            let proxy = proxy.clone();
            let db = db.clone();
            async move { execute_d1_guest_atomic(&db, proxy, req).await }
        };
        let exec = |op: &str, sql: &str| {
            run(ExecuteRequest {
                operation_id: op.into(),
                request_hash: String::new(),
                statements: vec![TypedDbStatement {
                    sql: sql.into(),
                    parameters: vec![],
                    kind: DbPlanStatementKind::Execute,
                    max_rows: 0,
                    result_selection: DbResultSelection::AffectedRows,
                }],
                deadline_unix_ms: 0,
            })
        };
        let sel = |op: &str, sql: &str| {
            run(ExecuteRequest {
                operation_id: op.into(),
                request_hash: String::new(),
                statements: vec![TypedDbStatement {
                    sql: sql.into(),
                    parameters: vec![],
                    kind: DbPlanStatementKind::Select,
                    max_rows: 8,
                    result_selection: DbResultSelection::Rows,
                }],
                deadline_unix_ms: 0,
            })
        };

        exec("d1-ddl-c", bookclerk_db_exec::sql_v1::BINDING_DDL_CONFLICT)
            .await
            .expect("conflict DDL");
        let first = exec(
            "d1-ins-u1",
            bookclerk_db_exec::sql_v1::PORTABLE_INSERT_OR_IGNORE_UNIQUE,
        )
        .await
        .expect("first unique insert");
        assert_eq!(first.statements[0].rows_affected, 1);
        let ignored = exec(
            "d1-ins-u2",
            bookclerk_db_exec::sql_v1::PORTABLE_INSERT_OR_IGNORE_UNIQUE,
        )
        .await
        .expect("duplicate unique ignore");
        assert_eq!(ignored.statements[0].rows_affected, 0);
        let err = exec(
            "d1-ins-nn",
            bookclerk_db_exec::sql_v1::PORTABLE_INSERT_OR_IGNORE_NOT_NULL,
        )
        .await
        .expect_err("NOT NULL must still abort");
        let t = err.to_string().to_ascii_lowercase();
        assert!(
            t.contains("null")
                || t.contains("constraint")
                || t.contains("not null")
                || t.contains("ambiguous")
                || t.contains("unavailable"),
            "{err}"
        );

        exec(
            "d1-ddl-typed",
            bookclerk_db_exec::sql_v1::BINDING_DDL_AUTOINCREMENT_BLOB,
        )
        .await
        .expect("typed DDL");
        run(ExecuteRequest {
            operation_id: "d1-ins-typed".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: bookclerk_db_exec::sql_v1::PORTABLE_INSERT.into(),
                parameters: vec![bookclerk_plugin_abi::DbValue::Bytes(
                    bookclerk_db_exec::sql_v1::PORTABLE_INSERT_BLOB.to_vec(),
                )],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::AffectedRows,
            }],
            deadline_unix_ms: 0,
        })
        .await
        .expect("typed insert");
        let mm = sel("d1-mm", bookclerk_db_exec::sql_v1::PORTABLE_MIN_MAX_NULL)
            .await
            .expect("min/max");
        if let Some(err) =
            bookclerk_db_exec::sql_v1::portable_min_max_null_mismatch(&mm.statements[0])
        {
            panic!("{err}");
        }
        let round = sel("d1-round", bookclerk_db_exec::sql_v1::PORTABLE_UNCAST_ROUND)
            .await
            .expect("round");
        if let Some(err) =
            bookclerk_db_exec::sql_v1::portable_uncast_round_mismatch(&round.statements[0])
        {
            panic!("{err}");
        }
        let sum_avg = sel(
            "d1-sum-avg",
            bookclerk_db_exec::sql_v1::PORTABLE_UNCAST_SUM_AVG,
        )
        .await
        .expect("sum/avg");
        if let Some(err) =
            bookclerk_db_exec::sql_v1::portable_uncast_sum_avg_mismatch(&sum_avg.statements[0])
        {
            panic!("{err}");
        }

        exec(
            "d1-ddl-ord",
            bookclerk_db_exec::sql_v1::BINDING_DDL_ORDER_NULLS,
        )
        .await
        .expect("order DDL");
        for (op, sql) in [
            (
                "d1-i1",
                bookclerk_db_exec::sql_v1::PORTABLE_ORDER_NULLS_INSERT_1,
            ),
            (
                "d1-inull",
                bookclerk_db_exec::sql_v1::PORTABLE_ORDER_NULLS_INSERT_NULL,
            ),
            (
                "d1-i2",
                bookclerk_db_exec::sql_v1::PORTABLE_ORDER_NULLS_INSERT_2,
            ),
        ] {
            exec(op, sql).await.expect(op);
        }
        let asc = sel(
            "d1-asc",
            bookclerk_db_exec::sql_v1::PORTABLE_ORDER_NULLS_ASC,
        )
        .await
        .expect("asc");
        if let Some(err) =
            bookclerk_db_exec::sql_v1::portable_order_nulls_asc_mismatch(&asc.statements[0])
        {
            panic!("{err}");
        }
        let desc = sel(
            "d1-desc",
            bookclerk_db_exec::sql_v1::PORTABLE_ORDER_NULLS_DESC,
        )
        .await
        .expect("desc");
        if let Some(err) =
            bookclerk_db_exec::sql_v1::portable_order_nulls_desc_mismatch(&desc.statements[0])
        {
            panic!("{err}");
        }

        exec("d1-ddl-id", bookclerk_db_exec::sql_v1::BINDING_DDL_IDENTITY)
            .await
            .expect("identity DDL");
        let err = run(ExecuteRequest {
            operation_id: "d1-ident-rollback".into(),
            request_hash: String::new(),
            statements: vec![
                TypedDbStatement {
                    sql: bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_INSERT_EXPLICIT.into(),
                    parameters: vec![],
                    kind: DbPlanStatementKind::Execute,
                    max_rows: 0,
                    result_selection: DbResultSelection::AffectedRows,
                },
                TypedDbStatement {
                    sql: bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_INSERT_EXPLICIT.into(),
                    parameters: vec![],
                    kind: DbPlanStatementKind::Execute,
                    max_rows: 0,
                    result_selection: DbResultSelection::AffectedRows,
                },
            ],
            deadline_unix_ms: 0,
        })
        .await
        .expect_err("unique conflict must abort");
        assert!(!err.to_string().is_empty(), "{err}");
        exec(
            "d1-ident-omit-rb",
            bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_INSERT_OMIT,
        )
        .await
        .expect("omit after rollback");
        let max_rb = sel(
            "d1-sel-max-rb",
            bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_SELECT_MAX,
        )
        .await
        .expect("max after rollback");
        assert_eq!(
            max_rb.statements[0].rows[0].values[0],
            bookclerk_plugin_abi::DbValue::Int64(1)
        );
        exec(
            "d1-ins-ex",
            bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_INSERT_EXPLICIT,
        )
        .await
        .expect("explicit id");
        exec(
            "d1-ins-om",
            bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_INSERT_OMIT,
        )
        .await
        .expect("omit id");
        let max1 = sel(
            "d1-sel-max1",
            bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_SELECT_MAX,
        )
        .await
        .expect("max after omit");
        assert_eq!(
            max1.statements[0].rows[0].values[0],
            bookclerk_plugin_abi::DbValue::Int64(101)
        );
        exec(
            "d1-del-max",
            bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_DELETE_MAX,
        )
        .await
        .expect("delete max");
        exec(
            "d1-ins-om2",
            bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_INSERT_OMIT,
        )
        .await
        .expect("omit after delete");
        let max2 = sel(
            "d1-sel-max2",
            bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_SELECT_MAX,
        )
        .await
        .expect("max after reinsert");
        assert_eq!(
            max2.statements[0].rows[0].values[0],
            bookclerk_plugin_abi::DbValue::Int64(102)
        );

        exec(
            "d1-ddl-ign",
            bookclerk_db_exec::sql_v1::BINDING_DDL_IGNORE_SELECT,
        )
        .await
        .expect("ign ddl");
        // D1 HTTP cannot prove `INSERT … SELECT … RETURNING` is `maxRows=1`.
        // The wrap still runs; results are not collected.
        for (op, sql, binds) in [
            (
                "d1-ign-sel",
                bookclerk_db_exec::sql_v1::PORTABLE_IGNORE_SELECT,
                vec![bookclerk_plugin_abi::DbValue::Int64(1)],
            ),
            (
                "d1-ign-with",
                bookclerk_db_exec::sql_v1::PORTABLE_IGNORE_SELECT_WITH,
                vec![bookclerk_plugin_abi::DbValue::Int64(2)],
            ),
            (
                "d1-ign-union",
                bookclerk_db_exec::sql_v1::PORTABLE_IGNORE_SELECT_UNION,
                vec![
                    bookclerk_plugin_abi::DbValue::Int64(3),
                    bookclerk_plugin_abi::DbValue::Int64(4),
                ],
            ),
            (
                "d1-ign-ord",
                bookclerk_db_exec::sql_v1::PORTABLE_IGNORE_SELECT_ORDER_LIMIT,
                vec![bookclerk_plugin_abi::DbValue::Int64(5)],
            ),
        ] {
            run(ExecuteRequest {
                operation_id: op.into(),
                request_hash: String::new(),
                statements: vec![TypedDbStatement {
                    sql: sql.replace(" RETURNING id", ""),
                    parameters: binds,
                    kind: DbPlanStatementKind::Execute,
                    max_rows: 0,
                    result_selection: DbResultSelection::AffectedRows,
                }],
                deadline_unix_ms: 0,
            })
            .await
            .expect(op);
        }

        exec("d1-ddl-like", bookclerk_db_exec::sql_v1::BINDING_DDL_LIKE)
            .await
            .expect("like ddl");
        exec(
            "d1-ins-like",
            bookclerk_db_exec::sql_v1::PORTABLE_LIKE_INSERT,
        )
        .await
        .expect("like ins");
        let liked = run(ExecuteRequest {
            operation_id: "d1-sel-like".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: bookclerk_db_exec::sql_v1::PORTABLE_LIKE_SELECT.into(),
                parameters: vec![bookclerk_plugin_abi::DbValue::Text("A".into())],
                kind: DbPlanStatementKind::Select,
                max_rows: 8,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        })
        .await
        .expect("like sel");
        if let Some(err) = bookclerk_db_exec::sql_v1::portable_statement_mismatch(
            &liked.statements[0],
            bookclerk_db_exec::sql_v1::portable_like_expects(),
            "like",
        ) {
            panic!("{err}");
        }
        let na = sel(
            "d1-sel-like-na",
            bookclerk_db_exec::sql_v1::PORTABLE_LIKE_NON_ASCII,
        )
        .await
        .expect("like na");
        assert_eq!(
            na.statements[0].rows[0].values[0],
            bookclerk_plugin_abi::DbValue::Int64(0)
        );

        exec(
            "d1-ddl-blobdef",
            bookclerk_db_exec::sql_v1::BINDING_DDL_BLOB_DEFAULT,
        )
        .await
        .expect("blobdef ddl");
        exec(
            "d1-ins-blobdef",
            bookclerk_db_exec::sql_v1::PORTABLE_BLOB_DEFAULT_INSERT,
        )
        .await
        .expect("blobdef ins");
        let blobdef = sel(
            "d1-sel-blobdef",
            bookclerk_db_exec::sql_v1::PORTABLE_BLOB_DEFAULT_SELECT,
        )
        .await
        .expect("blobdef sel");
        assert_eq!(
            blobdef.statements[0].rows[0].values[0],
            bookclerk_plugin_abi::DbValue::Bytes(vec![0xde, 0xad, 0xbe, 0xef])
        );

        exec(
            "d1-ddl-textord",
            bookclerk_db_exec::sql_v1::BINDING_DDL_TEXT_ORDER,
        )
        .await
        .expect("textord ddl");
        for (op, sql) in [
            (
                "d1-to-b",
                bookclerk_db_exec::sql_v1::PORTABLE_TEXT_ORDER_INSERT_B,
            ),
            (
                "d1-to-a",
                bookclerk_db_exec::sql_v1::PORTABLE_TEXT_ORDER_INSERT_A,
            ),
            (
                "d1-to-eac",
                bookclerk_db_exec::sql_v1::PORTABLE_TEXT_ORDER_INSERT_EACUTE,
            ),
            (
                "d1-to-e",
                bookclerk_db_exec::sql_v1::PORTABLE_TEXT_ORDER_INSERT_E,
            ),
        ] {
            exec(op, sql).await.expect(op);
        }
        let ordered = sel(
            "d1-text-ord",
            bookclerk_db_exec::sql_v1::PORTABLE_TEXT_ORDER_SELECT,
        )
        .await
        .expect("text ord");
        if let Some(err) = bookclerk_db_exec::sql_v1::portable_rows_mismatch(
            &ordered.statements[0],
            bookclerk_db_exec::sql_v1::portable_text_order_expects(),
            "text order",
        ) {
            panic!("{err}");
        }
        let tops = sel("d1-text-ops", bookclerk_db_exec::sql_v1::PORTABLE_TEXT_OPS)
            .await
            .expect("text ops");
        if let Some(err) = bookclerk_db_exec::sql_v1::portable_statement_mismatch(
            &tops.statements[0],
            bookclerk_db_exec::sql_v1::portable_text_ops_expects(),
            "text ops",
        ) {
            panic!("{err}");
        }
        let multi = sel(
            "d1-text-multi",
            bookclerk_db_exec::sql_v1::PORTABLE_TEXT_MULTILINGUAL,
        )
        .await
        .expect("text multilingual");
        if let Some(err) = bookclerk_db_exec::sql_v1::portable_statement_mismatch(
            &multi.statements[0],
            bookclerk_db_exec::sql_v1::portable_text_multilingual_expects(),
            "text multilingual",
        ) {
            panic!("{err}");
        }

        exec(
            "d1-ident-drop",
            bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_DROP,
        )
        .await
        .expect("drop ident");
        exec(
            "d1-ident-recreate",
            bookclerk_db_exec::sql_v1::BINDING_DDL_IDENTITY,
        )
        .await
        .expect("recreate ident");
        exec(
            "d1-ident-omit-re",
            bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_INSERT_OMIT,
        )
        .await
        .expect("omit after recreate");
        let max_re = sel(
            "d1-sel-max-re",
            bookclerk_db_exec::sql_v1::PORTABLE_IDENTITY_SELECT_MAX,
        )
        .await
        .expect("max after recreate");
        assert_eq!(
            max_re.statements[0].rows[0].values[0],
            bookclerk_plugin_abi::DbValue::Int64(1)
        );

        exec(
            "d1-ddl-fold",
            bookclerk_db_exec::sql_v1::BINDING_DDL_UNQUOTED_FOLD,
        )
        .await
        .expect("fold DDL");
        exec(
            "d1-ins-fold",
            bookclerk_db_exec::sql_v1::PORTABLE_UNQUOTED_FOLD_INSERT,
        )
        .await
        .expect("fold insert");
        let folded = sel(
            "d1-sel-fold",
            bookclerk_db_exec::sql_v1::PORTABLE_UNQUOTED_FOLD_SELECT,
        )
        .await
        .expect("fold select");
        assert_eq!(
            folded.statements[0].rows[0].values[0],
            bookclerk_plugin_abi::DbValue::Int64(7)
        );

        let ov = sel(
            "d1-overflow",
            bookclerk_db_exec::sql_v1::PORTABLE_INTEGER_OVERFLOW,
        )
        .await
        .expect("integer overflow");
        if let Some(err) =
            bookclerk_db_exec::sql_v1::portable_integer_overflow_mismatch(&ov.statements[0])
        {
            panic!("{err}");
        }
        let nested = sel(
            "d1-nested-arith",
            bookclerk_db_exec::sql_v1::PORTABLE_NESTED_INTEGER_ARITH,
        )
        .await
        .expect("nested integer arith");
        if let Some(err) =
            bookclerk_db_exec::sql_v1::portable_nested_integer_arith_mismatch(&nested.statements[0])
        {
            panic!("{err}");
        }
        let nested_b = run(ExecuteRequest {
            operation_id: "d1-nested-arith-binds".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "SELECT ? + abs(?)".into(),
                parameters: vec![
                    bookclerk_plugin_abi::DbValue::Int64(1),
                    bookclerk_plugin_abi::DbValue::Int64(-2),
                ],
                kind: DbPlanStatementKind::Select,
                max_rows: 8,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        })
        .await
        .expect("nested arith binds");
        assert_eq!(
            nested_b.statements[0].rows[0].values[0],
            bookclerk_plugin_abi::DbValue::Int64(3)
        );
        let path_cmp = sel(
            "d1-json-path-like-compare",
            bookclerk_db_exec::sql_v1::PORTABLE_JSON_PATH_LIKE_COMPARE,
        )
        .await
        .expect("json-path-like compare");
        if let Some(err) = bookclerk_db_exec::sql_v1::portable_json_path_like_compare_mismatch(
            &path_cmp.statements[0],
        ) {
            panic!("{err}");
        }
        let div = sel(
            "d1-div-operands",
            bookclerk_db_exec::sql_v1::PORTABLE_DIV_OPERANDS,
        )
        .await
        .expect("div operands");
        if let Some(err) =
            bookclerk_db_exec::sql_v1::portable_div_operands_mismatch(&div.statements[0])
        {
            panic!("{err}");
        }
        exec(
            "d1-div-ddl",
            "CREATE TABLE IF NOT EXISTS divops (n INTEGER)",
        )
        .await
        .expect("divops ddl");
        exec("d1-div-ins", "INSERT INTO divops (n) VALUES (0)")
            .await
            .expect("divops insert");
        let qdiv = sel(
            "d1-div-qual",
            "SELECT 10 / abs(n) AS d0, 10 / t.n AS d1, 10 / -n AS d2, 10 / CAST(n AS INTEGER) AS d3, 10 / (n + 0) AS d4 FROM divops t",
        )
        .await
        .expect("qualified div");
        assert!(
            qdiv.statements[0].rows[0]
                .values
                .iter()
                .all(|v| matches!(v, bookclerk_plugin_abi::DbValue::Null(_))),
            "{:?}",
            qdiv.statements[0].rows[0].values
        );
        let prefixes = sel(
            "d1-text-prefixes",
            bookclerk_db_exec::sql_v1::PORTABLE_TEXT_PREFIX_LITERALS,
        )
        .await
        .expect("text prefixes");
        if let Some(err) = bookclerk_db_exec::sql_v1::portable_text_prefix_literals_mismatch(
            &prefixes.statements[0],
        ) {
            panic!("{err}");
        }
    }

    /// A direct (unwrapped, non-receipt-gated) typed mutation whose reply is
    /// lost after commit must NOT be resubmitted: state changes exactly once,
    /// only one HTTP batch carries the mutation, and the caller receives the
    /// documented ambiguous → `unavailable` outcome.
    #[tokio::test]
    async fn executing_mock_direct_typed_mutation_lost_reply_is_not_resubmitted() {
        use bookclerk_plugin_sdk::{
            DbPlanStatementKind, DbResultSelection, ExecuteRequest, PluginErrorCode,
            TypedDbStatement,
        };

        let (server, proxy, conn, _interrupt, drop_reply, _oversize) = executing_proxy().await;
        proxy
            .run_batch(&[("CREATE TABLE direct_ops (k TEXT)".into(), Vec::new())])
            .await
            .expect("create table");
        let before = server.received_requests().await.unwrap().len();
        drop_reply.store(true, std::sync::atomic::Ordering::SeqCst);
        let req = ExecuteRequest {
            operation_id: "direct-lost".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "INSERT INTO direct_ops (k) VALUES ('x')".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::AffectedRows,
            }],
            deadline_unix_ms: 0,
        };
        let err = proxy
            .run_typed_atomic(
                &req,
                bookclerk_plugin_abi::GuestReceiptPersist::default(),
                &[],
            )
            .await
            .expect_err("lost reply on an unwrapped mutation must not be retried");
        assert!(crate::atomic::is_ambiguous_d1(&err), "{err}");
        let mapped = crate::atomic::plugin_error_from_d1(err);
        assert_eq!(mapped.code, PluginErrorCode::Unavailable, "{mapped}");
        let count: i64 = conn
            .lock()
            .expect("sqlite")
            .query_row("SELECT COUNT(*) FROM direct_ops", [], |row| row.get(0))
            .expect("count rows");
        assert_eq!(count, 1, "mutation must commit exactly once");
        let after = server.received_requests().await.unwrap().len();
        assert_eq!(
            after - before,
            1,
            "no second HTTP batch may carry the mutation"
        );
    }

    /// A guest `UPDATE … RETURNING` that can affect two rows but claims
    /// `maxRows = 1` is rejected during host authorization — before any
    /// adapter HTTP — and leaves state unchanged.
    #[tokio::test]
    async fn executing_mock_guest_update_returning_claim_rejected_before_execution() {
        use bookclerk_library::GuestSqlPolicy;
        use bookclerk_plugin_sdk::{
            DbPlanStatementKind, DbResultSelection, ExecuteRequest, PluginErrorCode,
            TypedDbStatement,
        };

        let (server, proxy, conn, _interrupt, _drop, _oversize) = executing_proxy().await;
        let db = sea_orm::Database::connect_proxy(
            DatabaseBackend::Sqlite,
            std::sync::Arc::new(Box::new(proxy.clone())),
        )
        .await
        .unwrap();
        let proxy_for_batch = proxy.clone();
        bookclerk_library::apply_host_schema_with_batch(
            &db,
            bookclerk_library::HostSchemaKind::AtomicBatchMarker,
            move |stmts| {
                let proxy = proxy_for_batch.clone();
                async move { run_schema_batch(proxy, stmts).await }
            },
        )
        .await
        .expect("host schema");
        proxy
            .run_batch(&[
                (
                    "CREATE TABLE claims (id INTEGER, v TEXT)".into(),
                    Vec::new(),
                ),
                (
                    "INSERT INTO claims VALUES (1, 'a'), (2, 'a')".into(),
                    Vec::new(),
                ),
            ])
            .await
            .expect("seed rows");

        let store = bookclerk_library::LibraryStore::from_connection(db)
            .with_db_capabilities(bookclerk_plugin_abi::DbCapabilities::advertised_d1())
            .with_typed_exec(std::sync::Arc::new(ProxyTypedExec {
                proxy: proxy.clone(),
            }));
        let policy = GuestSqlPolicy::allow_tables(["claims", "db_atomic_receipts"]);
        let before = server.received_requests().await.unwrap().len();
        let req = ExecuteRequest {
            operation_id: "claimed-bound".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "UPDATE claims SET v = 'b' RETURNING id".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Returning,
                max_rows: 1,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        };
        let err = store
            .execute_guest_atomic(req, &policy)
            .await
            .expect_err("guest-asserted bound must be rejected before execution");
        assert_eq!(err.code, PluginErrorCode::InvalidParams, "{err}");
        let after = server.received_requests().await.unwrap().len();
        assert_eq!(
            after, before,
            "no adapter HTTP may run for a rejected claim"
        );
        let changed: i64 = conn
            .lock()
            .expect("sqlite")
            .query_row("SELECT COUNT(*) FROM claims WHERE v = 'b'", [], |row| {
                row.get(0)
            })
            .expect("count");
        assert_eq!(changed, 0, "state must be unchanged");
    }

    /// A read-only typed request whose reply is lost after HTTP success is
    /// safe to resubmit: the retry succeeds without caller-visible failure.
    #[tokio::test]
    async fn executing_mock_read_only_lost_reply_is_retried() {
        use bookclerk_plugin_sdk::{
            DbPlanStatementKind, DbResultSelection, DbValue, ExecuteRequest, TypedDbStatement,
        };

        let (_server, proxy, _conn, _interrupt, drop_reply, _oversize) = executing_proxy().await;
        drop_reply.store(true, std::sync::atomic::Ordering::SeqCst);
        let req = ExecuteRequest {
            operation_id: "read-lost".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "SELECT 7 AS n".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Select,
                max_rows: 0,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        };
        let reply = proxy
            .run_typed_atomic(
                &req,
                bookclerk_plugin_abi::GuestReceiptPersist::default(),
                &[],
            )
            .await
            .expect("read-only lost reply must be retried transparently");
        assert_eq!(reply.statements[0].rows[0].values[0], DbValue::Int64(7));
    }
}
