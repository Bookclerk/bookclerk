//! Bookclerk Cap'n Proto stdio adapter over the workerd HTTP bridge.
//!
//! Toward the host this process is a [`PluginWorker`]. The control plane
//! (`describe` / `open` / `shutdown`) is a JSON policy handshake with the
//! adapter isolate; every ABI *method* call on an exported entrypoint is one
//! `POST /invoke` carrying Cap'n Proto bytes ([`crate::invoke`]), so the typed
//! clients from `bookclerk-plugin-abi` (`ContentSourceClient`,
//! `EventConsumerClient`, `JobRunnerClient`, `DatabaseClient`, …) are reused
//! unchanged over the [`InvokeClient`] hook. Object bodies stream over the
//! dedicated `/destination/get` / `/destination/put` routes. The exported
//! [`Entrypoints`] follow the signed `plugin.toml` capabilities (the host
//! allowlist), never a guest-declared widening. See `docs/workerd-bridge.md`.
//!
//! With a [`Backend::Native`] guest the isolate stays the control plane
//! (`describe` merge, `open` policy, `shutdown`) while every entrypoint
//! capability is the guest's own typed Cap'n Proto client, forwarded to the
//! host without entering JavaScript memory.

#![allow(clippy::missing_docs_in_private_items)]
#![allow(clippy::arc_with_non_send_sync)]

use std::collections::BTreeMap;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use async_trait::async_trait;
use bookclerk_plugin_abi::{
    require_plugin_migration_registration, GuestSqlPolicy, PluginError, Result as AbiResult,
};
use bookclerk_plugin_abi::{
    serve_plugin_stdio, Bindings, ByteRange, ContentSource, ContentSourceClient, CopyResult,
    Database, DatabaseClient, Destination, DestinationClient, Entrypoint, Entrypoints,
    EventConsumer, EventConsumerClient, ExtensibleConfig, GuestDatabase, HostBindings, Invocation,
    JobController, JobOutcome, JobRunner, JobRunnerClient, ListOptions, ListPage, ObjectMetadata,
    Oidc, OidcClient, OpenedEntrypoints, PluginCapabilities, PluginCli, PluginCliClient,
    PluginClient, PluginDescribe, PluginMigration, PluginWorker, PutResult, ReadResult,
    RemoteLibrary, RemoteLibraryClient, WriteOptions, MAX_SCALAR_BYTES, MAX_STREAM_WINDOW_BYTES,
    PRODUCT_API_VERSION,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::AsyncRead;

use crate::bridge_http::BridgeHttp;
use crate::granted::{GrantedSlot, GrantedTable};
use crate::invoke::{CapDescriptor, GrantCap, InvokeClient};

/// Data plane behind the workerd control plane.
pub enum Backend {
    /// Author isolate (`runtime = "workerd"`): every entrypoint call is a
    /// Cap'n Proto `POST /invoke` (or a streamed body route) on the bridge
    /// worker.
    Author,
    /// Verified native guest (`BOOKCLERK_NATIVE_BACKEND`): entrypoint calls
    /// are typed Cap'n Proto forwards to the guest's own capabilities. The
    /// client must have been created with `connect_plugin` on this `LocalSet`
    /// and its RPC future spawned locally.
    Native(PluginClient),
}

/// Serves Bookclerk capnp on stdio while talking HTTP/JSRPC to the isolate.
///
/// Must run inside a `LocalSet` (same thread as the granted HTTP server).
/// `capabilities` come from the signed manifest and decide which
/// [`Entrypoints`] `open` exports; `backend` selects the data plane.
///
/// # Errors
///
/// Returns a plugin error when the vat fails.
pub async fn mediate_bridge_stdio(
    http: BridgeHttp,
    table: GrantedTable,
    capabilities: PluginCapabilities,
    backend: Backend,
) -> anyhow::Result<()> {
    let plugin = WorkerdRoot {
        http,
        table,
        capabilities,
        backend,
    };
    serve_plugin_stdio(Arc::new(plugin), MAX_STREAM_WINDOW_BYTES)
        .await
        .map_err(|err| anyhow::anyhow!("{err}"))
}

struct WorkerdRoot {
    http: BridgeHttp,
    table: GrantedTable,
    capabilities: PluginCapabilities,
    backend: Backend,
}

/// Wire name of the `[[events.consumers]]` trigger family on `/open`.
const FAMILY_EVENT_CONSUMER: &str = "eventConsumer";
/// Wire name of the `[triggers] jobs` family on `/open`.
const FAMILY_JOB_RUNNER: &str = "jobRunner";

impl WorkerdRoot {
    fn exports(&self, entrypoint: Entrypoint) -> bool {
        self.capabilities.entrypoints.contains(&entrypoint)
    }

    /// Storage guests run the host `stream_copy` job even without an explicit
    /// `[triggers] jobs` list.
    fn runs_jobs(&self) -> bool {
        !self.capabilities.jobs.is_empty() || self.exports(Entrypoint::Storage)
    }

    /// Entrypoint families the signed manifest lets this plugin export; the
    /// adapter isolate filters the same list again against `PLUGIN_DESCRIBE`.
    fn manifest_families(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.capabilities.consumes.is_empty() {
            out.push(FAMILY_EVENT_CONSUMER);
        }
        if self.runs_jobs() {
            out.push(FAMILY_JOB_RUNNER);
        }
        for entrypoint in [
            Entrypoint::Storefront,
            Entrypoint::Storage,
            Entrypoint::DatabaseAdapter,
            Entrypoint::RemoteLibrary,
            Entrypoint::Cli,
            Entrypoint::Oidc,
        ] {
            if self.exports(entrypoint) {
                out.push(entrypoint.wire_name());
            }
        }
        out
    }

    /// Native `describe`: the guest's typed describe, merged by the adapter
    /// isolate against the manifest projection (`PLUGIN_DESCRIBE`).
    async fn describe_native(&self, client: &PluginClient) -> AbiResult<PluginDescribe> {
        let native = client.describe().await?;
        call(
            &self.http,
            "/describe",
            &serde_json::json!({ "native": to_bridge_json(&native)? }),
        )
        .await
    }

    /// Native `open`: the adapter isolate authorizes the invocation and the
    /// entrypoint families, then the guest's typed capabilities are forwarded
    /// as the exported [`Entrypoints`] (anything not authorized is nulled).
    async fn open_native(
        &self,
        client: &PluginClient,
        invocation: Invocation,
        bindings: Bindings,
    ) -> AbiResult<Entrypoints> {
        // The isolate only decides policy here; secrets stay on the typed
        // Cap'n path to the native guest.
        let ctx = BridgeContext {
            invocation: invocation.clone(),
            config: bindings.config.clone(),
            secrets: ExtensibleConfig::default(),
            events_token: None,
            databases: BTreeMap::new(),
        };
        let requested = self.manifest_families();
        let reply: OpenReply = call(
            &self.http,
            "/open",
            &OpenCall {
                context: &ctx,
                entrypoints: &requested,
            },
        )
        .await?;
        let allowed: Vec<&str> = reply
            .entrypoints
            .iter()
            .map(String::as_str)
            .filter(|name| requested.contains(name))
            .collect();
        let host_bindings = HostBindings {
            values: bindings.values(),
            events: bindings.events.map(Arc::from),
            databases: bindings
                .databases
                .into_iter()
                .map(|(name, database)| (name, Arc::from(database)))
                .collect(),
            cancel: Arc::from(bindings.cancel),
            storage: bindings.storage.map(Arc::from),
        };
        let opened = client.open(&invocation, host_bindings).await?;
        Ok(native_entrypoints(opened, &allowed))
    }

    /// Author `open`: mint the `EVENTS` grant, then export one typed client
    /// per manifest family over a shared [`InvokeClient`] carrying this
    /// invocation's bridge context.
    fn open_author(&self, invocation: Invocation, bindings: Bindings) -> AbiResult<Entrypoints> {
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
        // One database-only grant token per named `[[databases]]` binding:
        // the adapter isolate reaches each isolated plugin database over
        // `/db/execute` with its binding token and installs a D1-shaped
        // binding on the author's `env` for every invocation of this `open`.
        let mut databases = BTreeMap::new();
        let mut database_grants = Vec::with_capacity(bindings.databases.len());
        for (name, database) in bindings.databases {
            let token = format!("{:032x}", rand::random::<u128>());
            self.table.borrow_mut().insert(
                token.clone(),
                GrantedSlot::database_only(
                    Rc::from(database),
                    grant_expiry(invocation.deadline_unix_ms),
                ),
            );
            database_grants.push(RevokeGrant {
                table: Rc::clone(&self.table),
                grant: token.clone(),
            });
            databases.insert(name, token);
        }
        let ctx = BridgeContext {
            invocation,
            config: bindings.config,
            secrets: bindings.secrets,
            events_token,
            databases,
        };
        let ctx_json = context_header(&ctx)?;
        let grants = Rc::new(OpenGrants {
            _events: events_grant,
            _databases: database_grants,
        });
        let keepalive: Option<Rc<dyn std::any::Any>> =
            Some(Rc::clone(&grants) as Rc<dyn std::any::Any>);
        let invoke =
            InvokeClient::with_keepalive(self.http.clone(), Some(ctx_json.clone()), keepalive);

        let mut exported = Entrypoints::default();
        if self.exports(Entrypoint::Storefront) {
            exported.storefront = Some(Box::new(ContentSourceClient::new(invoke.typed())));
        }
        if self.exports(Entrypoint::Storage) {
            exported.storage = Some(Box::new(InvokeDestination {
                http: self.http.clone(),
                ctx: Rc::from(ctx_json.as_str()),
                typed: DestinationClient::new(invoke.typed(), MAX_STREAM_WINDOW_BYTES),
            }));
        }
        if self.exports(Entrypoint::DatabaseAdapter) {
            exported.database_adapter = Some(Box::new(DatabaseClient::new(invoke.typed())));
        }
        if self.exports(Entrypoint::RemoteLibrary) {
            exported.remote_library = Some(Box::new(RemoteLibraryClient::new(invoke.typed())));
        }
        if !self.capabilities.consumes.is_empty() {
            exported.event_consumer = Some(Box::new(EventConsumerClient::new(invoke.typed())));
        }
        if self.exports(Entrypoint::Oidc) {
            exported.oidc = Some(Box::new(OidcClient::new(invoke.typed())));
        }
        if self.exports(Entrypoint::Cli) {
            exported.cli = Some(Box::new(PluginCliClient::new(invoke.typed())));
        }
        if self.runs_jobs() {
            exported.job_runner = Some(Box::new(InvokeJobRunner {
                http: self.http.clone(),
                ctx,
                table: Rc::clone(&self.table),
                _grants: grants,
            }));
        }
        Ok(exported)
    }
}

/// Boxes the guest's typed clients as the exported [`Entrypoints`], keeping
/// only the families in `allowed`.
fn native_entrypoints(opened: OpenedEntrypoints, allowed: &[&str]) -> Entrypoints {
    let allow = |name: &str| allowed.contains(&name);
    let OpenedEntrypoints {
        event_consumer,
        job_runner,
        storefront,
        storage,
        database_adapter,
        remote_library,
        cli,
        oidc,
    } = opened;
    Entrypoints {
        event_consumer: event_consumer
            .filter(|_| allow(FAMILY_EVENT_CONSUMER))
            .map(|c| Box::new(c) as Box<dyn EventConsumer>),
        job_runner: job_runner
            .filter(|_| allow(FAMILY_JOB_RUNNER))
            .map(|c| Box::new(c) as Box<dyn JobRunner>),
        storefront: storefront
            .filter(|_| allow(Entrypoint::Storefront.wire_name()))
            .map(|c| Box::new(c) as Box<dyn ContentSource>),
        storage: storage
            .filter(|_| allow(Entrypoint::Storage.wire_name()))
            .map(|c| Box::new(c) as Box<dyn Destination>),
        database_adapter: database_adapter
            .filter(|_| allow(Entrypoint::DatabaseAdapter.wire_name()))
            .map(|c| Box::new(c) as Box<dyn Database>),
        remote_library: remote_library
            .filter(|_| allow(Entrypoint::RemoteLibrary.wire_name()))
            .map(|c| Box::new(c) as Box<dyn RemoteLibrary>),
        cli: cli
            .filter(|_| allow(Entrypoint::Cli.wire_name()))
            .map(|c| Box::new(c) as Box<dyn PluginCli>),
        oidc: oidc
            .filter(|_| allow(Entrypoint::Oidc.wire_name()))
            .map(|c| Box::new(c) as Box<dyn Oidc>),
    }
}

/// `POST /open` body: the open context plus the families the launcher asks
/// the adapter isolate to authorize.
#[derive(Serialize)]
struct OpenCall<'a> {
    context: &'a BridgeContext,
    entrypoints: &'a [&'static str],
}

/// `POST /open` reply: the authorized subset of the requested families.
#[derive(serde::Deserialize)]
struct OpenReply {
    #[serde(default)]
    entrypoints: Vec<String>,
}

/// Bridge projection of one `PluginWorker.open`: the `Invocation` envelope
/// plus the granted `CONFIG` / `SECRETS` bindings, carried as the `context`
/// field of `/open` and the `x-bookclerk-context` header of every `/invoke`
/// and stream request, which the isolate-side SDK installs on the author's
/// `env`. `eventsToken` is the granted-channel bearer the adapter isolate
/// turns into the author's `EVENTS` binding, and `databases` maps each named
/// `[[databases]]` binding to its database-only bearer for `/db/execute`;
/// the author sees neither token, only the bindings the adapter builds.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BridgeContext {
    invocation: Invocation,
    config: ExtensibleConfig,
    secrets: ExtensibleConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    events_token: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    databases: BTreeMap<String, String>,
}

/// `JobRunner.job` context: the durable job id on top of the open context.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JobBridgeContext<'a> {
    job_id: &'a str,
    #[serde(flatten)]
    base: &'a BridgeContext,
}

/// Grants minted at `open` that live as long as any exported entrypoint of
/// that `open` (the `InvokeClient` keepalive and the job runner both hold
/// this).
struct OpenGrants {
    _events: Option<Rc<RevokeGrant>>,
    _databases: Vec<RevokeGrant>,
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
/// as base64), used by the control plane only. The isolate side decodes with
/// the same generated types.
fn to_bridge_json<T: Serialize>(value: &T) -> AbiResult<serde_json::Value> {
    serde_json::to_value(value)
        .map_err(|err| PluginError::internal(format!("bridge request encode failed: {err}")))
}

fn from_bridge_json<T: DeserializeOwned>(path: &str, value: serde_json::Value) -> AbiResult<T> {
    serde_json::from_value(value).map_err(|err| {
        PluginError::internal(format!("bridge reply for {path} is malformed: {err}"))
    })
}

/// POST a control-plane envelope and decode its JSON reply.
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

/// `x-bookclerk-context` header value: the typed context as bridge JSON.
fn context_header<C: Serialize>(ctx: &C) -> AbiResult<String> {
    serde_json::to_string(ctx)
        .map_err(|err| PluginError::internal(format!("context encode failed: {err}")))
}

#[async_trait(?Send)]
impl PluginWorker for WorkerdRoot {
    async fn describe(&self) -> AbiResult<PluginDescribe> {
        let describe: PluginDescribe = match &self.backend {
            Backend::Author => call(&self.http, "/describe", &serde_json::json!({})).await?,
            Backend::Native(client) => self.describe_native(client).await?,
        };
        if describe.api_version != PRODUCT_API_VERSION {
            return Err(PluginError::unsupported(format!(
                "unsupported apiVersion {}",
                describe.api_version
            )));
        }
        Ok(describe)
    }

    async fn open(&self, invocation: Invocation, bindings: Bindings) -> AbiResult<Entrypoints> {
        match &self.backend {
            Backend::Native(client) => self.open_native(client, invocation, bindings).await,
            Backend::Author => self.open_author(invocation, bindings),
        }
    }

    /// `PluginWorker.databaseMigrations`: typed forward to the native guest,
    /// or the same Cap'n envelope on `/invoke` (no context: this is a
    /// worker-level call, not an entrypoint call).
    async fn database_migrations(&self, binding: &str) -> AbiResult<Vec<PluginMigration>> {
        let migrations = match &self.backend {
            Backend::Native(client) => client.database_migrations(binding).await?,
            Backend::Author => {
                let worker = PluginClient::new(
                    InvokeClient::new(self.http.clone(), None).typed(),
                    MAX_STREAM_WINDOW_BYTES,
                );
                worker.database_migrations(binding).await?
            }
        };
        require_plugin_migration_registration(&migrations)?;
        Ok(migrations)
    }

    /// Fans `shutdown` out to the native guest (when present) and to the
    /// adapter isolate's `shutdown` hook; both run even if one fails.
    async fn shutdown(&self) -> AbiResult<()> {
        let native = match &self.backend {
            Backend::Native(client) => client.shutdown().await,
            Backend::Author => Ok(()),
        };
        let bridge = self
            .http
            .json_post("/shutdown", &serde_json::json!({}))
            .await
            .map(|_| ())
            .map_err(map_http);
        native.and(bridge)
    }
}

/// `storage` entrypoint of an author isolate: scalar methods ride `/invoke`
/// through the typed [`DestinationClient`]; object bodies stream over the
/// dedicated routes so they never enter a Cap'n message.
struct InvokeDestination {
    http: BridgeHttp,
    /// `x-bookclerk-context` for the stream routes.
    ctx: Rc<str>,
    typed: DestinationClient,
}

#[async_trait(?Send)]
impl Destination for InvokeDestination {
    async fn head(&self, key: &str) -> AbiResult<Option<ObjectMetadata>> {
        self.typed.head(key).await
    }

    async fn list(&self, options: ListOptions) -> AbiResult<ListPage> {
        self.typed.list(options).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> AbiResult<ReadResult> {
        let mut path = format!("/destination/get?key={}", percent_encode(key));
        if let Some(r) = range {
            path.push_str(&format!("&offset={}", r.offset));
            if let Some(len) = r.length {
                path.push_str(&format!("&length={len}"));
            }
        }
        let (meta, body) = self
            .http
            .get_stream_headers(&path, &[("x-bookclerk-context", &self.ctx)])
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
        let mut extra = vec![("x-bookclerk-context", &*self.ctx)];
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
        self.typed.copy(from, to).await
    }

    async fn delete(&self, key: &str) -> AbiResult<()> {
        self.typed.delete(key).await
    }

    async fn commit(&self, key: &str, commit_token: &str) -> AbiResult<PutResult> {
        self.typed.commit(key, commit_token).await
    }

    async fn abort_stage(&self, key: &str, commit_token: &str) -> AbiResult<()> {
        self.typed.abort_stage(key, commit_token).await
    }
}

/// `jobRunner` entrypoint of an author isolate. Each run mints one grant
/// token for the host `JobController` stubs (input / output / progress /
/// cancel), which travel in the `JobRunner.job$Params` capability table as
/// [`GrantCap`] descriptors; named database bindings ride the open context.
struct InvokeJobRunner {
    http: BridgeHttp,
    ctx: BridgeContext,
    table: GrantedTable,
    /// Keeps the `EVENTS` and database grants alive while the job runner is
    /// exported.
    _grants: Rc<OpenGrants>,
}

#[async_trait(?Send)]
impl JobRunner for InvokeJobRunner {
    async fn job(&self, controller: JobController) -> AbiResult<JobOutcome> {
        let JobController {
            invocation,
            input,
            output,
            progress,
            cancel,
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
                cancel: Some(cancel),
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
        let ctx_json = context_header(&JobBridgeContext {
            job_id: &self.ctx.invocation.id,
            base: &self.ctx,
        })?;
        let runner = JobRunnerClient::new(
            InvokeClient::new(self.http.clone(), Some(ctx_json)).typed(),
            MAX_STREAM_WINDOW_BYTES,
        );
        let token = || grant.clone();
        runner
            .job_with_capabilities(
                &invocation,
                GrantCap::client(CapDescriptor::Source { token: token() }),
                GrantCap::client(CapDescriptor::Destination { token: token() }),
                GrantCap::client(CapDescriptor::Progress { token: token() }),
                GrantCap::client(CapDescriptor::Cancellation { token: token() }),
            )
            .await
    }
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

    #[test]
    fn job_context_flattens_the_open_context() {
        let base = BridgeContext {
            invocation: Invocation {
                id: "open-1".into(),
                ..Invocation::default()
            },
            config: ExtensibleConfig::default(),
            secrets: ExtensibleConfig::default(),
            events_token: Some("ev".into()),
            databases: BTreeMap::from([("DB".to_string(), "tok".to_string())]),
        };
        let json = context_header(&JobBridgeContext {
            job_id: &base.invocation.id,
            base: &base,
        })
        .expect("encode");
        let v: serde_json::Value = serde_json::from_str(&json).expect("json");
        assert_eq!(v["jobId"], "open-1");
        assert_eq!(v["invocation"]["id"], "open-1");
        assert_eq!(v["eventsToken"], "ev");
        assert_eq!(v["databases"]["DB"], "tok");
        assert!(v.get("config").is_some());
    }
}
