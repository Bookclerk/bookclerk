//! Bookclerk Cap'n Proto stdio adapter over the workerd HTTP bridge.
//!
//! Isolates keep `RpcTarget` stubs; this process maps them onto the host-facing
//! [`PluginRoot`] / [`Destination`] traits with streamed HTTP bodies.

#![allow(clippy::missing_docs_in_private_items)]
#![allow(clippy::arc_with_non_send_sync)]

use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use async_trait::async_trait;
use bookclerk_plugin_abi::{
    require_plugin_migration_json_lists, require_plugin_migration_registration, GuestSqlPolicy,
    PluginError, Result as AbiResult,
};
use bookclerk_plugin_abi::{
    serve_plugin_stdio, AuthenticateUserParams, ByteRange, CatalogDetailParams, CatalogHit,
    CliInvokeParams, CliInvokeResult, CliSchema, ContentSource, ContentSourceContext, CopyResult,
    Destination, DestinationContext, DomainEvent, EventPollResult, EventResult,
    ExpandCandidatesParams, ExternalUser, FetchTitleParams, GuestDatabase, HealthOk, Integration,
    IntegrationContext, JobHandler, JobHandlerContext, JobInvocation, JobOutcome, ListDealsParams,
    ListOptions, ListPage, ListeningProgress, LoginCompleteParams, LoginParams, LoginResult,
    LoginStartResult, ObjectInfo, ObjectMetadata, OidcClientTemplate, PlainFetch, PluginDescribe,
    PluginMigration, PluginRoot, PurchaseHint, PurchaseHintParams, PutResult, ReadResult,
    ScanLibraryParams, ScanParams, ScanSummary, SearchCatalogParams, Source, SourceAccount,
    SourceContext, WorkerContext, WriteOptions, MAX_LIST_PAGE, MAX_SCALAR_BYTES,
    MAX_STREAM_WINDOW_BYTES, PRODUCT_API_VERSION,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::AsyncRead;

use crate::bridge_http::BridgeHttp;
use crate::granted::{GrantedSlot, GrantedTable};

/// Serves Bookclerk capnp on stdio while talking HTTP/JSRPC to the isolate.
///
/// Must run inside a `LocalSet` (same thread as the granted HTTP server).
///
/// # Errors
///
/// Returns a plugin error when the vat fails.
pub async fn mediate_bridge_stdio(http: BridgeHttp, table: GrantedTable) -> anyhow::Result<()> {
    let plugin = WorkerdRoot { http, table };
    serve_plugin_stdio(Arc::new(plugin), MAX_STREAM_WINDOW_BYTES)
        .await
        .map_err(|err| anyhow::anyhow!("{err}"))
}

struct WorkerdRoot {
    http: BridgeHttp,
    table: GrantedTable,
}

fn map_http(err: anyhow::Error) -> PluginError {
    let msg = err.to_string();
    if let Some((code, rest)) = msg.split_once(": ") {
        return PluginError::from_wire(code, rest);
    }
    PluginError::internal(msg)
}

/// Bridge JSON is the camelCase projection of the typed Cap'n structs (bytes
/// as base64). The isolate side decodes with the same generated types.
fn to_bridge_json<T: Serialize>(value: &T) -> AbiResult<serde_json::Value> {
    serde_json::to_value(value)
        .map_err(|err| PluginError::internal(format!("bridge request encode failed: {err}")))
}

fn from_bridge_json<T: DeserializeOwned>(path: &str, value: serde_json::Value) -> AbiResult<T> {
    serde_json::from_value(value).map_err(|err| {
        PluginError::internal(format!("bridge reply for {path} is malformed: {err}"))
    })
}

/// POST a typed envelope and decode a typed reply.
async fn call<P: Serialize, R: DeserializeOwned>(
    http: &BridgeHttp,
    path: &str,
    body: &P,
) -> AbiResult<R> {
    let reply = http
        .json_post(path, &to_bridge_json(body)?)
        .await
        .map_err(map_http)?;
    from_bridge_json(path, reply)
}

/// `{ context, params }` envelope for role method routes.
#[derive(Serialize)]
struct RoleCall<'a, C: Serialize, P: Serialize> {
    context: &'a C,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<&'a P>,
}

/// Guests may answer `diagnose` with a bare array or `{ lines }`.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum DiagnoseReply {
    Lines(Vec<String>),
    Object { lines: Vec<String> },
}

impl From<DiagnoseReply> for Vec<String> {
    fn from(reply: DiagnoseReply) -> Self {
        match reply {
            DiagnoseReply::Lines(lines) | DiagnoseReply::Object { lines } => lines,
        }
    }
}

/// `{ ok: true }` / `{}` acknowledgements for unit-returning methods.
#[derive(serde::Deserialize)]
struct Ack {}

#[async_trait(?Send)]
impl PluginRoot for WorkerdRoot {
    async fn describe(&self) -> AbiResult<PluginDescribe> {
        let describe: PluginDescribe =
            call(&self.http, "/describe", &serde_json::json!({})).await?;
        if describe.api_version != PRODUCT_API_VERSION {
            return Err(PluginError::unsupported(format!(
                "unsupported apiVersion {}",
                describe.api_version
            )));
        }
        Ok(describe)
    }

    async fn destination(&self, context: DestinationContext) -> AbiResult<Box<dyn Destination>> {
        Ok(Box::new(HttpDestination {
            http: self.http.clone(),
            ctx: context,
        }))
    }

    async fn source(&self, context: SourceContext) -> AbiResult<Box<dyn Source>> {
        Ok(Box::new(HttpSource {
            http: self.http.clone(),
            ctx: context,
        }))
    }

    async fn worker(&self, context: WorkerContext) -> AbiResult<Box<dyn JobHandler>> {
        Ok(Box::new(HttpJobHandler {
            http: self.http.clone(),
            ctx: context,
            table: Rc::clone(&self.table),
        }))
    }

    async fn content_source(
        &self,
        context: ContentSourceContext,
    ) -> AbiResult<Box<dyn ContentSource>> {
        Ok(Box::new(HttpContentSource {
            http: self.http.clone(),
            ctx: context,
        }))
    }

    async fn integration(&self, context: IntegrationContext) -> AbiResult<Box<dyn Integration>> {
        Ok(Box::new(HttpIntegration {
            http: self.http.clone(),
            ctx: context,
        }))
    }

    async fn cli_describe(&self) -> AbiResult<CliSchema> {
        call(&self.http, "/cliDescribe", &serde_json::json!({})).await
    }

    async fn cli_invoke(&self, params: CliInvokeParams) -> AbiResult<CliInvokeResult> {
        call(
            &self.http,
            "/cliInvoke",
            &serde_json::json!({ "params": to_bridge_json(&params)? }),
        )
        .await
    }

    async fn oidc_clients(&self) -> AbiResult<Vec<OidcClientTemplate>> {
        #[derive(serde::Deserialize)]
        struct Reply {
            #[serde(default)]
            clients: Vec<OidcClientTemplate>,
        }
        let reply: Reply = call(&self.http, "/oidcClients", &serde_json::json!({})).await?;
        Ok(reply.clients)
    }

    async fn database_migrations(&self, binding: &str) -> AbiResult<Vec<PluginMigration>> {
        let v = self
            .http
            .json_post(
                "/databaseMigrations",
                &serde_json::json!({ "binding": binding }),
            )
            .await
            .map_err(map_http)?;
        let migrations = v
            .get("migrations")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([]));
        require_plugin_migration_json_lists(&migrations)?;
        let parsed: Vec<PluginMigration> = serde_json::from_value(migrations)
            .map_err(|err| PluginError::internal(err.to_string()))?;
        require_plugin_migration_registration(&parsed)?;
        Ok(parsed)
    }
}

struct HttpDestination {
    http: BridgeHttp,
    ctx: DestinationContext,
}

struct HttpIntegration {
    http: BridgeHttp,
    ctx: IntegrationContext,
}

impl HttpIntegration {
    async fn op<P: Serialize, R: DeserializeOwned>(
        &self,
        op: &str,
        params: Option<&P>,
    ) -> AbiResult<R> {
        call(
            &self.http,
            &format!("/integration/{op}"),
            &RoleCall {
                context: &self.ctx,
                params,
            },
        )
        .await
    }
}

const NO_PARAMS: Option<&()> = None;

#[async_trait(?Send)]
impl Integration for HttpIntegration {
    async fn health(&self) -> AbiResult<HealthOk> {
        self.op("health", NO_PARAMS).await
    }

    async fn diagnose(&self) -> AbiResult<Vec<String>> {
        let reply: DiagnoseReply = self.op("diagnose", NO_PARAMS).await?;
        Ok(reply.into())
    }

    async fn on_event(&self, event: DomainEvent) -> AbiResult<EventResult> {
        let v = self
            .http
            .json_post(
                "/integration/onEvent",
                &serde_json::json!({
                    "context": to_bridge_json(&self.ctx)?,
                    "event": to_bridge_json(&event)?,
                }),
            )
            .await
            .map_err(map_http)?;
        EventResult::from_json_value(&v)
    }

    async fn start(&self) -> AbiResult<()> {
        let _: Ack = self.op("start", NO_PARAMS).await?;
        Ok(())
    }

    async fn stop(&self) -> AbiResult<()> {
        let _: Ack = self.op("stop", NO_PARAMS).await?;
        Ok(())
    }

    async fn scan_library(&self, params: ScanLibraryParams) -> AbiResult<()> {
        let _: Ack = self.op("scanLibrary", Some(&params)).await?;
        Ok(())
    }

    async fn sync_listening(&self) -> AbiResult<Vec<ListeningProgress>> {
        self.op("syncListening", NO_PARAMS).await
    }

    async fn authenticate_user(&self, params: AuthenticateUserParams) -> AbiResult<ExternalUser> {
        self.op("authenticateUser", Some(&params)).await
    }

    async fn poll_events(&self) -> AbiResult<Vec<ExternalUser>> {
        let reply: EventPollResult = self.op("pollEvents", NO_PARAMS).await?;
        Ok(reply.users)
    }
}

struct HttpContentSource {
    http: BridgeHttp,
    ctx: ContentSourceContext,
}

impl HttpContentSource {
    async fn op<P: Serialize, R: DeserializeOwned>(
        &self,
        op: &str,
        params: Option<&P>,
    ) -> AbiResult<R> {
        call(
            &self.http,
            &format!("/contentSource/{op}"),
            &RoleCall {
                context: &self.ctx,
                params,
            },
        )
        .await
    }
}

#[async_trait(?Send)]
impl ContentSource for HttpContentSource {
    async fn login(&self, params: LoginParams) -> AbiResult<LoginResult> {
        self.op("login", Some(&params)).await
    }

    async fn scan(&self, params: ScanParams) -> AbiResult<ScanSummary> {
        self.op("scan", Some(&params)).await
    }

    async fn fetch_title(&self, params: FetchTitleParams) -> AbiResult<PlainFetch> {
        self.op("fetchTitle", Some(&params)).await
    }

    async fn list_accounts(&self) -> AbiResult<Vec<SourceAccount>> {
        self.op("listAccounts", NO_PARAMS).await
    }

    async fn login_start(&self, params: LoginParams) -> AbiResult<LoginStartResult> {
        self.op("loginStart", Some(&params)).await
    }

    async fn login_complete(&self, params: LoginCompleteParams) -> AbiResult<LoginResult> {
        self.op("loginComplete", Some(&params)).await
    }

    async fn search_catalog(&self, params: SearchCatalogParams) -> AbiResult<Vec<CatalogHit>> {
        self.op("searchCatalog", Some(&params)).await
    }

    async fn expand_candidates(
        &self,
        params: ExpandCandidatesParams,
    ) -> AbiResult<Vec<CatalogHit>> {
        self.op("expandCandidates", Some(&params)).await
    }

    async fn purchase_hint(&self, params: PurchaseHintParams) -> AbiResult<Option<PurchaseHint>> {
        self.op("purchaseHint", Some(&params)).await
    }

    async fn list_deals(&self, params: ListDealsParams) -> AbiResult<Vec<CatalogHit>> {
        self.op("listDeals", Some(&params)).await
    }

    async fn catalog_detail(&self, params: CatalogDetailParams) -> AbiResult<Option<CatalogHit>> {
        self.op("catalogDetail", Some(&params)).await
    }

    async fn health(&self) -> AbiResult<HealthOk> {
        self.op("health", NO_PARAMS).await
    }

    async fn diagnose(&self) -> AbiResult<Vec<String>> {
        let reply: DiagnoseReply = self.op("diagnose", NO_PARAMS).await?;
        Ok(reply.into())
    }
}

/// `x-bookclerk-context` header value: the typed context as bridge JSON.
fn context_header<C: Serialize>(ctx: &C) -> AbiResult<String> {
    serde_json::to_string(ctx)
        .map_err(|err| PluginError::internal(format!("context encode failed: {err}")))
}

#[async_trait(?Send)]
impl Destination for HttpDestination {
    async fn head(&self, key: &str) -> AbiResult<Option<ObjectMetadata>> {
        let v = self
            .http
            .json_post(
                "/destination/head",
                &serde_json::json!({ "key": key, "context": to_bridge_json(&self.ctx)? }),
            )
            .await
            .map_err(map_http)?;
        if v.get("found").and_then(|x| x.as_bool()) != Some(true) {
            return Ok(None);
        }
        Ok(v.get("meta").and_then(meta_from_json))
    }

    async fn list(&self, options: ListOptions) -> AbiResult<ListPage> {
        let v = self
            .http
            .json_post(
                "/destination/list",
                &serde_json::json!({
                    "context": to_bridge_json(&self.ctx)?,
                    "options": {
                        "prefix": options.prefix,
                        "cursor": options.cursor,
                        "limit": options.limit,
                    },
                }),
            )
            .await
            .map_err(map_http)?;
        let objects: Vec<ObjectInfo> = v
            .get("objects")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|o| {
                        Some(ObjectInfo {
                            key: o.get("key")?.as_str()?.to_string(),
                            size: o.get("size")?.as_u64().unwrap_or(0),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        if objects.len() > MAX_LIST_PAGE as usize {
            return Err(PluginError::payload_too_large(format!(
                "list page of {} objects exceeds {MAX_LIST_PAGE}",
                objects.len()
            )));
        }
        Ok(ListPage {
            objects,
            next_cursor: v
                .get("nextCursor")
                .and_then(|x| x.as_str())
                .map(str::to_string),
        })
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> AbiResult<ReadResult> {
        let mut path = format!("/destination/get?key={}", percent_encode(key));
        if let Some(r) = range {
            path.push_str(&format!("&offset={}", r.offset));
            if let Some(len) = r.length {
                path.push_str(&format!("&length={len}"));
            }
        }
        let ctx = context_header(&self.ctx)?;
        let (meta, body) = self
            .http
            .get_stream_headers(&path, &[("x-bookclerk-context", ctx.as_str())])
            .await
            .map_err(map_http)?;
        Ok(ReadResult { meta, body })
    }

    async fn put(
        &self,
        key: &str,
        body: Pin<Box<dyn AsyncRead + Send>>,
        options: WriteOptions,
    ) -> AbiResult<PutResult> {
        let path = format!("/destination/put?key={}", percent_encode(key));
        let ctx = context_header(&self.ctx)?;
        let mut extra = vec![("x-bookclerk-context", ctx.as_str())];
        let token;
        if let Some(t) = &options.commit_token {
            token = t.clone();
            extra.push(("x-bookclerk-commit-token", token.as_str()));
        }
        let stage = if options.stage_only { "1" } else { "0" };
        extra.push(("x-bookclerk-stage-only", stage));
        self.http
            .put_stream_headers(
                &path,
                body,
                options.content_type.as_deref(),
                options.content_length,
                &extra,
            )
            .await
            .map_err(map_http)
    }

    async fn copy(&self, from: &str, to: &str) -> AbiResult<CopyResult> {
        let v = self
            .http
            .json_post(
                "/destination/copy",
                &serde_json::json!({ "from": from, "to": to, "context": to_bridge_json(&self.ctx)? }),
            )
            .await
            .map_err(map_http)?;
        Ok(CopyResult {
            bytes_copied: v.get("bytesCopied").and_then(|x| x.as_u64()).unwrap_or(0),
        })
    }

    async fn delete(&self, key: &str) -> AbiResult<()> {
        self.http
            .json_post(
                "/destination/delete",
                &serde_json::json!({ "key": key, "context": to_bridge_json(&self.ctx)? }),
            )
            .await
            .map_err(map_http)?;
        Ok(())
    }

    async fn commit(&self, key: &str, commit_token: &str) -> AbiResult<PutResult> {
        let v = self
            .http
            .json_post(
                "/destination/commit",
                &serde_json::json!({
                    "key": key,
                    "commitToken": commit_token,
                    "context": to_bridge_json(&self.ctx)?,
                }),
            )
            .await
            .map_err(map_http)?;
        Ok(PutResult {
            key: v
                .get("key")
                .and_then(|x| x.as_str())
                .unwrap_or(key)
                .to_string(),
            bytes_written: v.get("bytesWritten").and_then(|x| x.as_u64()).unwrap_or(0),
            etag: v.get("etag").and_then(|x| x.as_str()).map(str::to_string),
            sha256: None,
        })
    }

    async fn abort_stage(&self, key: &str, commit_token: &str) -> AbiResult<()> {
        self.http
            .json_post(
                "/destination/abortStage",
                &serde_json::json!({
                    "key": key,
                    "commitToken": commit_token,
                    "context": to_bridge_json(&self.ctx)?,
                }),
            )
            .await
            .map_err(map_http)?;
        Ok(())
    }
}

struct HttpSource {
    http: BridgeHttp,
    ctx: SourceContext,
}

#[async_trait(?Send)]
impl Source for HttpSource {
    async fn open(&self, key: &str) -> AbiResult<ReadResult> {
        let path = format!("/source/open?key={}", percent_encode(key));
        let ctx = context_header(&self.ctx)?;
        let (meta, body) = self
            .http
            .get_stream_headers(&path, &[("x-bookclerk-context", ctx.as_str())])
            .await
            .map_err(map_http)?;
        Ok(ReadResult { meta, body })
    }
}

struct HttpJobHandler {
    http: BridgeHttp,
    ctx: WorkerContext,
    table: GrantedTable,
}

#[async_trait(?Send)]
impl JobHandler for HttpJobHandler {
    async fn handle(
        &self,
        invocation: JobInvocation,
        context: JobHandlerContext,
    ) -> AbiResult<JobOutcome> {
        let (allow_database, max_request_bytes) =
            granted_database_budget(context.database.as_deref());
        let grant = format!("{:032x}", rand::random::<u128>());
        self.table.borrow_mut().insert(
            grant.clone(),
            GrantedSlot {
                input: Some(context.input),
                output: Some(context.output),
                progress: Some(context.progress),
                expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
                allow_open: true,
                allow_put: true,
                allow_progress: true,
                database: context.database.map(Rc::from),
                allow_database,
                sql_policy: GuestSqlPolicy::host_authoritative(),
                max_request_bytes,
            },
        );
        let _revoke = RevokeGrant {
            table: Rc::clone(&self.table),
            grant: grant.clone(),
        };
        // One database-only grant token per named plugin database binding:
        // the isolate reaches each isolated database over the same
        // `/db/execute` broker path with its binding token.
        let mut binding_tokens = serde_json::Map::new();
        let mut binding_revokes = Vec::with_capacity(context.databases.len());
        for (name, database) in context.databases {
            let token = format!("{:032x}", rand::random::<u128>());
            self.table.borrow_mut().insert(
                token.clone(),
                GrantedSlot {
                    input: None,
                    output: None,
                    progress: None,
                    expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
                    allow_open: false,
                    allow_put: false,
                    allow_progress: false,
                    database: Some(Rc::from(database)),
                    allow_database: true,
                    // The host-side binding session enforces binding_owned
                    // scope; the broker defers to it.
                    sql_policy: GuestSqlPolicy::host_authoritative(),
                    max_request_bytes: MAX_SCALAR_BYTES,
                },
            );
            binding_revokes.push(RevokeGrant {
                table: Rc::clone(&self.table),
                grant: token.clone(),
            });
            binding_tokens.insert(name, serde_json::Value::String(token));
        }
        let result = self
            .http
            .json_post(
                "/worker/handle",
                &serde_json::json!({
                    "grantToken": grant,
                    "databases": binding_tokens,
                    "invocation": invocation,
                    "context": to_bridge_json(&self.ctx)?,
                }),
            )
            .await;
        let v = result.map_err(map_http)?;
        outcome_from_json(&v)
    }
}

fn meta_from_json(v: &serde_json::Value) -> Option<ObjectMetadata> {
    Some(ObjectMetadata {
        key: v.get("key")?.as_str()?.to_string(),
        size: v.get("size")?.as_u64().unwrap_or(0),
        content_type: v
            .get("contentType")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        etag: v.get("etag").and_then(|x| x.as_str()).map(str::to_string),
        sha256: None,
    })
}

fn outcome_from_json(v: &serde_json::Value) -> AbiResult<JobOutcome> {
    let kind = v
        .get("kind")
        .and_then(|x| x.as_str())
        .or_else(|| {
            v.get("ok")
                .and_then(|ok| ok.as_bool())
                .and_then(|ok| ok.then_some("completed"))
        })
        .unwrap_or("completed");
    let message = v
        .get("message")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    Ok(match kind {
        "retryable" => JobOutcome::Retryable {
            message,
            retry_after_unix_ms: v.get("retryAfterUnixMs").and_then(|x| x.as_u64()),
        },
        "rejected" => JobOutcome::Rejected { message },
        "cancelled" => JobOutcome::Cancelled { message },
        "suspended" => JobOutcome::Suspended {
            checkpoint: bookclerk_plugin_abi::JobCheckpoint {
                schema_version: v
                    .get("checkpoint")
                    .and_then(|c| c.get("schemaVersion"))
                    .and_then(|x| x.as_u64())
                    .unwrap_or(1) as u32,
                json: v
                    .get("checkpoint")
                    .and_then(|c| c.get("json"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
            },
            wake_at_unix_ms: v.get("wakeAtUnixMs").and_then(|x| x.as_u64()).unwrap_or(0),
        },
        _ => JobOutcome::Completed {
            message,
            bytes_copied: v.get("bytesCopied").and_then(|x| x.as_u64()).unwrap_or(0),
        },
    })
}

/// Negotiates the granted `execute` body budget.
///
/// When no session is granted, the isolate cannot call `/db/execute`.
/// When a session is granted, the host scalar ceiling applies.
fn granted_database_budget(database: Option<&dyn GuestDatabase>) -> (bool, u32) {
    match database {
        None => (false, 0),
        Some(_) => (true, MAX_SCALAR_BYTES),
    }
}

struct RevokeGrant {
    table: GrantedTable,
    grant: String,
}

impl Drop for RevokeGrant {
    fn drop(&mut self) {
        self.table.borrow_mut().remove(&self.grant);
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use bookclerk_plugin_abi::ExecuteReply;

    struct BudgetSession;

    #[async_trait(?Send)]
    impl GuestDatabase for BudgetSession {
        async fn execute(
            &self,
            _request: bookclerk_plugin_abi::ExecuteRequest,
        ) -> AbiResult<ExecuteReply> {
            Err(PluginError::unsupported("execute"))
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn granted_budget_fails_closed_without_database() {
        assert_eq!(granted_database_budget(None), (false, 0));
        assert_eq!(
            granted_database_budget(Some(&BudgetSession)),
            (true, MAX_SCALAR_BYTES)
        );
    }
}
