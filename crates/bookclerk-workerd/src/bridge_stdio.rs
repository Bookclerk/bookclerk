//! Bookclerk Cap'n Proto stdio adapter over the workerd HTTP bridge.
//!
//! Isolates keep `RpcTarget` stubs; this process maps them onto the host-facing
//! [`PluginWorker`] entrypoint traits with streamed HTTP bodies. The exported
//! [`Entrypoints`] follow the signed `plugin.toml` capabilities (the host
//! allowlist), never a guest-declared widening.

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
    serve_plugin_stdio, AuthenticateUserParams, Bindings, ByteRange, CatalogDetailParams,
    CatalogHit, CliInvokeParams, CliInvokeResult, CliSchema, ContentSource, CopyResult,
    Destination, DomainEvent, Entrypoint, Entrypoints, EventConsumer, EventPollResult, EventResult,
    ExpandCandidatesParams, ExtensibleConfig, ExternalUser, FetchTitleParams, GuestDatabase,
    HealthOk, Invocation, JobController, JobOutcome, JobRunner, ListDealsParams, ListOptions,
    ListPage, ListeningProgress, LoginCompleteParams, LoginParams, LoginResult, LoginStartResult,
    ObjectInfo, ObjectMetadata, Oidc, OidcClientTemplate, PlainFetch, PluginCapabilities,
    PluginCli, PluginDescribe, PluginMigration, PluginWorker, PurchaseHint, PurchaseHintParams,
    PutResult, ReadResult, RemoteLibrary, ScanLibraryParams, ScanParams, ScanSummary,
    SearchCatalogParams, SourceAccount, WriteOptions, MAX_LIST_PAGE, MAX_SCALAR_BYTES,
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
/// `capabilities` come from the signed manifest and decide which
/// [`Entrypoints`] `open` exports.
///
/// # Errors
///
/// Returns a plugin error when the vat fails.
pub async fn mediate_bridge_stdio(
    http: BridgeHttp,
    table: GrantedTable,
    capabilities: PluginCapabilities,
) -> anyhow::Result<()> {
    let plugin = WorkerdRoot {
        http,
        table,
        capabilities,
    };
    serve_plugin_stdio(Arc::new(plugin), MAX_STREAM_WINDOW_BYTES)
        .await
        .map_err(|err| anyhow::anyhow!("{err}"))
}

struct WorkerdRoot {
    http: BridgeHttp,
    table: GrantedTable,
    capabilities: PluginCapabilities,
}

impl WorkerdRoot {
    fn exports(&self, entrypoint: Entrypoint) -> bool {
        self.capabilities.entrypoints.contains(&entrypoint)
    }

    /// Storage guests run the host `stream_copy` job even without an explicit
    /// `[triggers] jobs` list.
    fn runs_jobs(&self) -> bool {
        !self.capabilities.jobs.is_empty() || self.exports(Entrypoint::Storage)
    }
}

/// Bridge projection of one `PluginWorker.open`: the `Invocation` envelope
/// plus the granted `CONFIG` / `SECRETS` bindings, carried as the `context`
/// field / `x-bookclerk-context` header the isolate-side SDK installs on the
/// author's `env`. `eventsToken` is the granted-channel bearer the adapter
/// isolate turns into the author's `EVENTS` binding; the author never sees it.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BridgeContext {
    invocation: Invocation,
    config: ExtensibleConfig,
    secrets: ExtensibleConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    events_token: Option<String>,
    /// Keeps the events grant alive while any exported entrypoint holds it.
    #[serde(skip)]
    #[allow(dead_code)]
    events_grant: Option<Rc<RevokeGrant>>,
}

/// Job-runner bridge context: the durable job id plus the open context.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkerBridgeContext {
    job_id: String,
    invocation: Invocation,
    config: ExtensibleConfig,
    secrets: ExtensibleConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    events_token: Option<String>,
    /// Keeps the events grant alive while the job runner holds it.
    #[serde(skip)]
    #[allow(dead_code)]
    events_grant: Option<Rc<RevokeGrant>>,
}

/// Grant expiry for an `open`: the invocation deadline when it has one,
/// otherwise effectively the process lifetime (primary opens live as long as
/// the session).
fn grant_expiry(deadline_unix_ms: u64) -> std::time::Instant {
    let now = std::time::Instant::now();
    if deadline_unix_ms == 0 {
        return now + std::time::Duration::from_secs(10 * 365 * 24 * 3600);
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    now + std::time::Duration::from_millis(deadline_unix_ms.saturating_sub(now_ms))
        + std::time::Duration::from_secs(60)
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
impl PluginWorker for WorkerdRoot {
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

    async fn open(&self, invocation: Invocation, bindings: Bindings) -> AbiResult<Entrypoints> {
        // The host's `EVENTS` publisher becomes an events-only grant token the
        // adapter isolate exchanges on `/events/publish`; it is revoked when
        // the last exported entrypoint of this `open` drops.
        let (events_token, events_grant) = match bindings.events {
            Some(publisher) => {
                let token = format!("{:032x}", rand::random::<u128>());
                self.table.borrow_mut().insert(
                    token.clone(),
                    GrantedSlot::events_only(
                        Rc::from(publisher),
                        grant_expiry(invocation.deadline_unix_ms),
                    ),
                );
                (
                    Some(token.clone()),
                    Some(Rc::new(RevokeGrant {
                        table: Rc::clone(&self.table),
                        grant: token,
                    })),
                )
            }
            None => (None, None),
        };
        let ctx = BridgeContext {
            invocation: invocation.clone(),
            config: bindings.config.clone(),
            secrets: bindings.secrets.clone(),
            events_token: events_token.clone(),
            events_grant: events_grant.clone(),
        };
        let integration = || HttpIntegration {
            http: self.http.clone(),
            ctx: ctx.clone(),
        };
        let mut exported = Entrypoints::default();
        if self.exports(Entrypoint::Storefront) {
            exported.storefront = Some(Box::new(HttpContentSource {
                http: self.http.clone(),
                ctx: ctx.clone(),
            }));
        }
        if self.exports(Entrypoint::Storage) {
            exported.storage = Some(Box::new(HttpDestination {
                http: self.http.clone(),
                ctx: ctx.clone(),
            }));
        }
        if self.exports(Entrypoint::RemoteLibrary) {
            exported.remote_library = Some(Box::new(integration()));
        }
        if !self.capabilities.consumes.is_empty() {
            exported.event_consumer = Some(Box::new(integration()));
        }
        if self.exports(Entrypoint::Oidc) {
            exported.oidc = Some(Box::new(integration()));
        }
        if self.exports(Entrypoint::Cli) {
            exported.cli = Some(Box::new(HttpCli {
                http: self.http.clone(),
            }));
        }
        if self.runs_jobs() {
            exported.job_runner = Some(Box::new(HttpJobRunner {
                http: self.http.clone(),
                ctx: WorkerBridgeContext {
                    job_id: invocation.id.clone(),
                    invocation,
                    config: bindings.config,
                    secrets: bindings.secrets,
                    events_token,
                    events_grant,
                },
                databases: bindings
                    .databases
                    .into_iter()
                    .map(|(name, database)| (name, Rc::from(database)))
                    .collect(),
                table: Rc::clone(&self.table),
            }));
        }
        Ok(exported)
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

struct HttpCli {
    http: BridgeHttp,
}

#[async_trait(?Send)]
impl PluginCli for HttpCli {
    async fn describe(&self) -> AbiResult<CliSchema> {
        call(&self.http, "/cliDescribe", &serde_json::json!({})).await
    }

    async fn invoke(&self, params: CliInvokeParams) -> AbiResult<CliInvokeResult> {
        call(
            &self.http,
            "/cliInvoke",
            &serde_json::json!({ "params": to_bridge_json(&params)? }),
        )
        .await
    }
}

struct HttpDestination {
    http: BridgeHttp,
    ctx: BridgeContext,
}

/// `remoteLibrary` / event-consumer / `oidc` entrypoints share the
/// `/integration/{op}` bridge routes.
struct HttpIntegration {
    http: BridgeHttp,
    ctx: BridgeContext,
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
impl RemoteLibrary for HttpIntegration {
    async fn health(&self) -> AbiResult<HealthOk> {
        self.op("health", NO_PARAMS).await
    }

    async fn diagnose(&self) -> AbiResult<Vec<String>> {
        let reply: DiagnoseReply = self.op("diagnose", NO_PARAMS).await?;
        Ok(reply.into())
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

    async fn poll_events(&self) -> AbiResult<Vec<ExternalUser>> {
        let reply: EventPollResult = self.op("pollEvents", NO_PARAMS).await?;
        Ok(reply.users)
    }
}

#[async_trait(?Send)]
impl EventConsumer for HttpIntegration {
    async fn event(&self, batch: Vec<DomainEvent>) -> AbiResult<Vec<EventResult>> {
        let mut results = Vec::with_capacity(batch.len());
        for event in batch {
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
            results.push(EventResult::from_json_value(&v)?);
        }
        Ok(results)
    }
}

#[async_trait(?Send)]
impl Oidc for HttpIntegration {
    async fn clients(&self) -> AbiResult<Vec<OidcClientTemplate>> {
        #[derive(serde::Deserialize)]
        struct Reply {
            #[serde(default)]
            clients: Vec<OidcClientTemplate>,
        }
        let reply: Reply = call(&self.http, "/oidcClients", &serde_json::json!({})).await?;
        Ok(reply.clients)
    }

    async fn authenticate_user(&self, params: AuthenticateUserParams) -> AbiResult<ExternalUser> {
        self.op("authenticateUser", Some(&params)).await
    }
}

struct HttpContentSource {
    http: BridgeHttp,
    ctx: BridgeContext,
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

struct HttpJobRunner {
    http: BridgeHttp,
    ctx: WorkerBridgeContext,
    /// Named `[[databases]]` bindings granted at `open`; each job run mints a
    /// database-only grant token per binding.
    databases: Vec<(String, Rc<dyn GuestDatabase>)>,
    table: GrantedTable,
}

#[async_trait(?Send)]
impl JobRunner for HttpJobRunner {
    async fn job(&self, controller: JobController) -> AbiResult<JobOutcome> {
        let JobController {
            invocation,
            input,
            output,
            progress,
            cancel: _,
        } = controller;
        // Jobs never receive the host library as guest SQL; durable plugin
        // state uses the named bindings below.
        let (allow_database, max_request_bytes) = granted_database_budget(None);
        let grant = format!("{:032x}", rand::random::<u128>());
        self.table.borrow_mut().insert(
            grant.clone(),
            GrantedSlot {
                input: Some(input),
                output: Some(output),
                progress: Some(progress),
                expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
                allow_open: true,
                allow_put: true,
                allow_progress: true,
                database: None,
                allow_database,
                sql_policy: GuestSqlPolicy::host_authoritative(),
                max_request_bytes,
                events: None,
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
        let mut binding_revokes = Vec::with_capacity(self.databases.len());
        for (name, database) in &self.databases {
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
                    database: Some(Rc::clone(database)),
                    allow_database: true,
                    // The host-side binding session enforces binding_owned
                    // scope; the broker defers to it.
                    sql_policy: GuestSqlPolicy::host_authoritative(),
                    max_request_bytes: MAX_SCALAR_BYTES,
                    events: None,
                },
            );
            binding_revokes.push(RevokeGrant {
                table: Rc::clone(&self.table),
                grant: token.clone(),
            });
            binding_tokens.insert(name.clone(), serde_json::Value::String(token));
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
