//! Branded [`BookclerkPlugin`] guest trait + native Workers RPC guest runner.
//!
//! Audience: authors of **native** (`runtime = "native"`) guests. Implement
//! [`BookclerkPlugin`] for the methods your plugin kind needs, then call
//! [`BookclerkPluginGuest::serve`] from `main`. Workerd guests reuse the same
//! trait via [`crate::workerd`] / the npm package — they do not use the stdio
//! runner in this module.
//!
//! Wire names are camelCase Workers RPC methods (see `bookclerk-plugin-abi` and
//! [`crate::protocol::methods`]). Defaults return unsupported errors so unused
//! methods stay off the capability surface.

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use bookclerk_plugin_abi::{
    methods, AuthenticateUserParams, CatalogDetailParams, CatalogHitDto, CliInvokeParams,
    CliInvokeResult, CliSchema, CredentialsUpdateParams, DbAtomicRequest, DbAtomicResult,
    DbBeginParams, DbBeginResult, DbConnectParams, DbConnectResult, DbTxnParams, DiagnoseResult,
    EventPollResultDto, ExecResultDto, ExistsResultDto, ExpandCandidatesParams, ExternalUserDto,
    FetchTitleParams, GetResultDto, HandshakeParams, HandshakeResult, HealthResult,
    HostToPluginEvent, ListAccountsParams, ListDealsParams, LoginCompleteParams, LoginParams,
    LoginResultDto, LoginStartParams, LoginStartResultDto, ObjectInfoDto, ObjectProbeDto,
    OutputCopyParams, OutputGetParams, OutputKeyParams, OutputListParams, OutputPutFileParams,
    OutputPutParams, OutputTouchFileParams, PluginError, PluginErrorCode, PurchaseHintDto,
    PurchaseHintParams, QueryResultDto, RpcRequest, RpcResponse, ScanLibraryParams, ScanParams,
    ScanSummaryDto, SearchCatalogParams, SourceAccountDto, SourceFetchDto, StatementDto,
    SyncListeningResultDto, API_VERSION,
};

use crate::error::{Result, SdkError};
use crate::protocol::MAX_RPC_LINE_BYTES;

/// Branded guest contract — identical method surface for native and workerd SDKs.
///
/// Implement the methods your plugin kind needs; defaults return
/// [`PluginError::unsupported`]. Wire names are camelCase Workers RPC methods
/// (see `bookclerk-plugin-abi`).
#[async_trait]
pub trait BookclerkPlugin: Send + Sync + 'static {
    /// Negotiates ABI version and advertises plugin identity to the host.
    ///
    /// # Arguments
    ///
    /// * `params` - Host-provided handshake inputs (API version, install paths).
    ///
    /// # Returns
    ///
    /// Guest identity, kind, and negotiated capabilities.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the guest rejects the host ABI version.
    async fn handshake(
        &self,
        params: HandshakeParams,
    ) -> std::result::Result<HandshakeResult, PluginError>;

    /// Releases guest resources before the process exits.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when cleanup fails.
    async fn shutdown(&self) -> std::result::Result<(), PluginError> {
        Ok(())
    }

    /// Reports liveness for host health checks.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the guest cannot evaluate health.
    async fn health(&self) -> std::result::Result<HealthResult, PluginError> {
        Ok(HealthResult {
            ok: true,
            ..HealthResult::default()
        })
    }

    /// Collects operator-facing diagnostic lines for `plugins doctor`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when diagnostics cannot be gathered.
    async fn diagnose(&self) -> std::result::Result<DiagnoseResult, PluginError> {
        Ok(DiagnoseResult { lines: vec![] })
    }

    /// Starts long-running guest work after a successful handshake.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn start(&self) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported("start not implemented"))
    }

    /// Handles a host-to-plugin push event.
    ///
    /// # Arguments
    ///
    /// * `_event` - Event envelope from the host.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn on_event(&self, _event: HostToPluginEvent) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported("onEvent not implemented"))
    }

    /// Drains queued plugin-to-host events.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn poll_events(&self) -> std::result::Result<EventPollResultDto, PluginError> {
        Err(PluginError::unsupported("pollEvents not implemented"))
    }

    /// Scans the operator library through an integration guest.
    ///
    /// # Arguments
    ///
    /// * `_params` - Scan scope and account filters.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn scan_library(
        &self,
        _params: ScanLibraryParams,
    ) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported("scanLibrary not implemented"))
    }

    /// Syncs listening progress with an external library server.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn sync_listening(&self) -> std::result::Result<SyncListeningResultDto, PluginError> {
        Err(PluginError::unsupported("syncListening not implemented"))
    }

    /// Validates an external user identity for portal / OIDC flows.
    ///
    /// # Arguments
    ///
    /// * `_params` - Credentials or tokens from the connect portal.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn authenticate_user(
        &self,
        _params: AuthenticateUserParams,
    ) -> std::result::Result<ExternalUserDto, PluginError> {
        Err(PluginError::unsupported("authenticateUser not implemented"))
    }

    /// Describes guest CLI commands for `bookclerk plugin` / host plumbing.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the schema cannot be built.
    async fn cli_describe(&self) -> std::result::Result<CliSchema, PluginError> {
        Ok(CliSchema::default())
    }

    /// Invokes a guest CLI command described by [`Self::cli_describe`].
    ///
    /// # Arguments
    ///
    /// * `_params` - Command name and argv-style arguments.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn cli_invoke(
        &self,
        _params: CliInvokeParams,
    ) -> std::result::Result<CliInvokeResult, PluginError> {
        Err(PluginError::unsupported("cliInvoke not implemented"))
    }

    /// Performs a one-shot store login (password / token flows).
    ///
    /// # Arguments
    ///
    /// * `_params` - Store credentials and account labeling.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn login(
        &self,
        _params: LoginParams,
    ) -> std::result::Result<LoginResultDto, PluginError> {
        Err(PluginError::unsupported("login not implemented"))
    }

    /// Starts an interactive OAuth (or similar) login.
    ///
    /// # Arguments
    ///
    /// * `_params` - Marketplace / locale hints for the authorize URL.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn login_start(
        &self,
        _params: LoginStartParams,
    ) -> std::result::Result<LoginStartResultDto, PluginError> {
        Err(PluginError::unsupported("loginStart not implemented"))
    }

    /// Completes an interactive login started by [`Self::login_start`].
    ///
    /// # Arguments
    ///
    /// * `_params` - Callback payload / authorization code.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn login_complete(
        &self,
        _params: LoginCompleteParams,
    ) -> std::result::Result<LoginResultDto, PluginError> {
        Err(PluginError::unsupported("loginComplete not implemented"))
    }

    /// Updates stored credentials without a full login round-trip.
    ///
    /// # Arguments
    ///
    /// * `_params` - Account id and replacement secret material.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn credentials_update(
        &self,
        _params: CredentialsUpdateParams,
    ) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported(
            "credentialsUpdate not implemented",
        ))
    }

    /// Scans owned titles from a source storefront.
    ///
    /// # Arguments
    ///
    /// * `_params` - Account filters and pagination options.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn scan(&self, _params: ScanParams) -> std::result::Result<ScanSummaryDto, PluginError> {
        Err(PluginError::unsupported("scan not implemented"))
    }

    /// Downloads (and decrypts, when applicable) one title to a fetch directory.
    ///
    /// # Arguments
    ///
    /// * `_params` - Title id / ASIN / ISBN and destination hints.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn fetch_title(
        &self,
        _params: FetchTitleParams,
    ) -> std::result::Result<SourceFetchDto, PluginError> {
        Err(PluginError::unsupported("fetchTitle not implemented"))
    }

    /// Searches the storefront catalog.
    ///
    /// # Arguments
    ///
    /// * `_params` - Query string and optional filters.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn search_catalog(
        &self,
        _params: SearchCatalogParams,
    ) -> std::result::Result<Vec<CatalogHitDto>, PluginError> {
        Err(PluginError::unsupported("searchCatalog not implemented"))
    }

    /// Expands related catalog candidates for a seed title.
    ///
    /// # Arguments
    ///
    /// * `_params` - Seed identifiers and expansion limits.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn expand_candidates(
        &self,
        _params: ExpandCandidatesParams,
    ) -> std::result::Result<Vec<CatalogHitDto>, PluginError> {
        Err(PluginError::unsupported("expandCandidates not implemented"))
    }

    /// Returns a purchase / wishlist hint for a catalog title when available.
    ///
    /// # Arguments
    ///
    /// * `_params` - Title identifier in the storefront namespace.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn purchase_hint(
        &self,
        _params: PurchaseHintParams,
    ) -> std::result::Result<Option<PurchaseHintDto>, PluginError> {
        Err(PluginError::unsupported("purchaseHint not implemented"))
    }

    /// Lists current deals / sales from the storefront.
    ///
    /// # Arguments
    ///
    /// * `_params` - Pagination and marketplace filters.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn list_deals(
        &self,
        _params: ListDealsParams,
    ) -> std::result::Result<Vec<CatalogHitDto>, PluginError> {
        Err(PluginError::unsupported("listDeals not implemented"))
    }

    /// Lists connected source accounts known to this guest.
    ///
    /// # Arguments
    ///
    /// * `_params` - Optional account-id filter.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn list_accounts(
        &self,
        _params: ListAccountsParams,
    ) -> std::result::Result<Vec<SourceAccountDto>, PluginError> {
        Err(PluginError::unsupported("listAccounts not implemented"))
    }

    /// Fetches rich catalog detail for one title.
    ///
    /// # Arguments
    ///
    /// * `_params` - Storefront title identifier.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn catalog_detail(
        &self,
        _params: CatalogDetailParams,
    ) -> std::result::Result<Option<CatalogHitDto>, PluginError> {
        Err(PluginError::unsupported("catalogDetail not implemented"))
    }

    /// Writes bytes to a destination object key.
    ///
    /// # Arguments
    ///
    /// * `_params` - Object key and inline payload.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn put(&self, _params: OutputPutParams) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported("put not implemented"))
    }

    /// Uploads a local file to a destination object key.
    ///
    /// # Arguments
    ///
    /// * `_params` - Object key and absolute source path.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn put_file(&self, _params: OutputPutFileParams) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported("putFile not implemented"))
    }

    /// Reads an object from the destination.
    ///
    /// # Arguments
    ///
    /// * `_params` - Object key and optional byte range.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn get(
        &self,
        _params: OutputGetParams,
    ) -> std::result::Result<GetResultDto, PluginError> {
        Err(PluginError::unsupported("get not implemented"))
    }

    /// Checks whether an object key exists.
    ///
    /// # Arguments
    ///
    /// * `_params` - Object key.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn exists(
        &self,
        _params: OutputKeyParams,
    ) -> std::result::Result<ExistsResultDto, PluginError> {
        Err(PluginError::unsupported("exists not implemented"))
    }

    /// Lists objects under a destination prefix.
    ///
    /// # Arguments
    ///
    /// * `_params` - Prefix and pagination cursor.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn list(
        &self,
        _params: OutputListParams,
    ) -> std::result::Result<Vec<ObjectInfoDto>, PluginError> {
        Err(PluginError::unsupported("list not implemented"))
    }

    /// Probes object metadata without downloading the body.
    ///
    /// # Arguments
    ///
    /// * `_params` - Object key.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn probe(
        &self,
        _params: OutputKeyParams,
    ) -> std::result::Result<ObjectProbeDto, PluginError> {
        Err(PluginError::unsupported("probe not implemented"))
    }

    /// Copies an object from one key to another inside the destination.
    ///
    /// # Arguments
    ///
    /// * `_params` - Source and destination keys.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn copy(&self, _params: OutputCopyParams) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported("copy not implemented"))
    }

    /// Deletes an object key from the destination.
    ///
    /// # Arguments
    ///
    /// * `_params` - Object key.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn delete(&self, _params: OutputKeyParams) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported("delete not implemented"))
    }

    /// Updates timestamps / metadata on an existing local destination file.
    ///
    /// # Arguments
    ///
    /// * `_params` - Absolute path and touch options.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn touch_file(
        &self,
        _params: OutputTouchFileParams,
    ) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported("touchFile not implemented"))
    }

    /// Opens a database guest connection (SeaORM proxy / D1 / Postgres).
    ///
    /// # Arguments
    ///
    /// * `_params` - Backend-specific connect parameters.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn db_connect(
        &self,
        _params: DbConnectParams,
    ) -> std::result::Result<DbConnectResult, PluginError> {
        Err(PluginError::unsupported("dbConnect not implemented"))
    }

    /// Pings an open database guest connection.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn db_ping(&self) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported("dbPing not implemented"))
    }

    /// Runs a read query through the database guest.
    ///
    /// # Arguments
    ///
    /// * `_params` - SQL statement and bound values.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn db_query(
        &self,
        _params: StatementDto,
    ) -> std::result::Result<QueryResultDto, PluginError> {
        Err(PluginError::unsupported("dbQuery not implemented"))
    }

    /// Runs a write statement through the database guest.
    ///
    /// # Arguments
    ///
    /// * `_params` - SQL statement and bound values.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn db_execute(
        &self,
        _params: StatementDto,
    ) -> std::result::Result<ExecResultDto, PluginError> {
        Err(PluginError::unsupported("dbExecute not implemented"))
    }

    /// Begins a database transaction (or nested savepoint).
    ///
    /// # Arguments
    ///
    /// * `_params` - Optional parent transaction id for nested savepoints.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn db_begin(
        &self,
        _params: DbBeginParams,
    ) -> std::result::Result<DbBeginResult, PluginError> {
        Err(PluginError::unsupported("dbBegin not implemented"))
    }

    /// Commits a guest transaction returned by [`Self::db_begin`].
    ///
    /// # Arguments
    ///
    /// * `_params` - Transaction id to commit.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn db_commit(&self, _params: DbTxnParams) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported("dbCommit not implemented"))
    }

    /// Rolls back a guest transaction returned by [`Self::db_begin`].
    ///
    /// # Arguments
    ///
    /// * `_params` - Transaction id to roll back.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn db_rollback(&self, _params: DbTxnParams) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported("dbRollback not implemented"))
    }

    /// Runs a named atomic library operation as one guest SQL transaction.
    ///
    /// Bundled database guests implement this. D1 uses one HTTP `batch()`;
    /// SQLite and Postgres use a native local transaction. Both persist a
    /// receipt keyed by `operationId`.
    ///
    /// # Arguments
    ///
    /// * `_params` - Idempotency envelope and tagged operation.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] unless the guest overrides this.
    async fn db_atomic(
        &self,
        _params: DbAtomicRequest,
    ) -> std::result::Result<DbAtomicResult, PluginError> {
        Err(PluginError::unsupported("dbAtomic not implemented"))
    }

    /// Dispatches an unrecognized or future wire method.
    ///
    /// # Arguments
    ///
    /// * `method` - Workers RPC method name from the host.
    /// * `_params` - JSON params object for that method.
    ///
    /// # Returns
    ///
    /// JSON result value for the RPC response.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::unsupported`] for methods the guest does not implement.
    async fn call_raw(
        &self,
        method: &str,
        _params: Value,
    ) -> std::result::Result<Value, PluginError> {
        Err(PluginError::unsupported(format!(
            "method `{method}` not implemented"
        )))
    }
}

/// Native guest runner — hosts a [`BookclerkPlugin`] on stdin/stdout (Workers RPC).
///
/// Mirrors low-level [`crate::PluginGuest`], but dispatches to the branded trait
/// instead of a raw `(method, params)` closure. Workerd hosts the same trait
/// surface via isolate entrypoints instead of this type.
///
/// # Examples
///
/// ```ignore
/// use bookclerk_plugin_sdk::{BookclerkPlugin, BookclerkPluginGuest};
///
/// struct MyPlugin;
///
/// #[async_trait::async_trait]
/// impl BookclerkPlugin for MyPlugin {
///     async fn handshake(
///         &self,
///         params: bookclerk_plugin_sdk::HandshakeParams,
///     ) -> Result<bookclerk_plugin_sdk::HandshakeResult, bookclerk_plugin_sdk::PluginError> {
///         // negotiate api_version, return id/kind/capabilities
///         # let _ = params;
///         unimplemented!()
///     }
/// }
///
/// # async fn main_loop() -> bookclerk_plugin_sdk::Result<()> {
/// BookclerkPluginGuest::serve(MyPlugin).await?;
/// # Ok(())
/// # }
/// ```
pub struct BookclerkPluginGuest;

impl BookclerkPluginGuest {
    /// Runs the Workers RPC loop until stdin closes or `shutdown` succeeds.
    ///
    /// Reads newline-delimited JSON requests from tokio stdin, dispatches each
    /// method to the corresponding [`BookclerkPlugin`] trait method (or
    /// [`BookclerkPlugin::call_raw`] for unknown names), and writes JSON
    /// responses to stdout. Frames larger than
    /// [`crate::protocol::MAX_RPC_LINE_BYTES`] abort the loop. Handler errors on
    /// `shutdown` are coerced to a successful null result so the host can exit.
    ///
    /// # Arguments
    ///
    /// * `plugin` - Guest implementation to serve for the lifetime of this
    ///   process (typically until the host sends `shutdown`).
    ///
    /// # Returns
    ///
    /// `Ok(())` after stdin EOF or after flushing the `shutdown` response.
    ///
    /// # Errors
    ///
    /// Returns [`crate::SdkError`] on stdio I/O failure, oversize frames, or
    /// response serialization errors. Malformed request lines are logged and
    /// skipped without a response.
    pub async fn serve<P: BookclerkPlugin>(plugin: P) -> Result<()> {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut stdout = tokio::io::stdout();
        loop {
            let mut buf = Vec::new();
            let n = reader.read_until(b'\n', &mut buf).await?;
            if n == 0 {
                break;
            }
            if buf.len() > MAX_RPC_LINE_BYTES {
                return Err(SdkError::message("RPC line exceeds max size"));
            }
            let line = String::from_utf8_lossy(&buf);
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let req: RpcRequest = match serde_json::from_str(line) {
                Ok(r) => r,
                Err(err) => {
                    tracing::warn!(%err, "invalid Workers RPC request");
                    continue;
                }
            };
            let is_shutdown = req.method == methods::shutdown::NAME;
            let outcome = dispatch(&plugin, &req.method, req.params.unwrap_or(Value::Null)).await;
            let resp = match outcome {
                Ok(result) => RpcResponse {
                    id: req.id.clone(),
                    result: Some(result),
                    error: None,
                },
                Err(_err) if is_shutdown => RpcResponse {
                    id: req.id.clone(),
                    result: Some(Value::Null),
                    error: None,
                },
                Err(err) => RpcResponse {
                    id: req.id.clone(),
                    result: None,
                    error: Some(err),
                },
            };
            let mut out = serde_json::to_string(&resp)?;
            out.push('\n');
            stdout.write_all(out.as_bytes()).await?;
            stdout.flush().await?;
            if is_shutdown {
                break;
            }
        }
        Ok(())
    }
}

/// Deserializes an RPC `params` object into `T` for `method`.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when `params` cannot be deserialized
/// into the requested type.
fn parse_params<T: serde::de::DeserializeOwned>(
    method: &str,
    params: Value,
) -> std::result::Result<T, PluginError> {
    serde_json::from_value(params)
        .map_err(|e| PluginError::invalid_params(format!("{method} params: {e}")))
}

/// Serializes a plugin result into the JSON value placed on the RPC reply.
///
/// # Errors
///
/// Returns [`PluginError::internal`] when JSON serialization fails.
fn to_value<T: serde::Serialize>(value: T) -> std::result::Result<Value, PluginError> {
    serde_json::to_value(value).map_err(|e| PluginError::internal(e.to_string()))
}

/// Routes one host RPC method to the corresponding [`BookclerkPlugin`] trait method.
///
/// # Errors
///
/// Returns [`PluginError`] when parameter parsing, plugin method execution, or
/// result serialization fails, or when `method` is not recognized.
async fn dispatch<P: BookclerkPlugin>(
    plugin: &P,
    method: &str,
    params: Value,
) -> std::result::Result<Value, PluginError> {
    match method {
        m if m == methods::handshake::NAME => {
            let p: HandshakeParams = parse_params("handshake", params)?;
            if p.api_version != API_VERSION {
                return Err(PluginError::invalid_params(format!(
                    "unsupported apiVersion {}",
                    p.api_version
                )));
            }
            to_value(plugin.handshake(p).await?)
        }
        m if m == methods::shutdown::NAME => {
            plugin.shutdown().await?;
            Ok(Value::Null)
        }
        m if m == methods::health::NAME => to_value(plugin.health().await?),
        m if m == methods::diagnose::NAME => to_value(plugin.diagnose().await?),
        m if m == methods::start::NAME => {
            plugin.start().await?;
            Ok(Value::Null)
        }
        m if m == methods::on_event::NAME => {
            let event: HostToPluginEvent = parse_params("onEvent", params)?;
            plugin.on_event(event).await?;
            Ok(json!({ "ok": true }))
        }
        m if m == methods::poll_events::NAME => to_value(plugin.poll_events().await?),
        m if m == methods::scan_library::NAME => {
            let p: ScanLibraryParams = parse_params("scanLibrary", params)?;
            plugin.scan_library(p).await?;
            Ok(Value::Null)
        }
        m if m == methods::sync_listening::NAME => to_value(plugin.sync_listening().await?),
        m if m == methods::authenticate_user::NAME => {
            let p: AuthenticateUserParams = parse_params("authenticateUser", params)?;
            to_value(plugin.authenticate_user(p).await?)
        }
        m if m == methods::cli_describe::NAME => to_value(plugin.cli_describe().await?),
        m if m == methods::cli_invoke::NAME => {
            let p: CliInvokeParams = parse_params("cliInvoke", params)?;
            to_value(plugin.cli_invoke(p).await?)
        }
        m if m == methods::login::NAME => {
            let p: LoginParams = parse_params("login", params)?;
            to_value(plugin.login(p).await?)
        }
        m if m == methods::login_start::NAME => {
            let p: LoginStartParams = parse_params("loginStart", params)?;
            to_value(plugin.login_start(p).await?)
        }
        m if m == methods::login_complete::NAME => {
            let p: LoginCompleteParams = parse_params("loginComplete", params)?;
            to_value(plugin.login_complete(p).await?)
        }
        m if m == methods::credentials_update::NAME => {
            let p: CredentialsUpdateParams = parse_params("credentialsUpdate", params)?;
            plugin.credentials_update(p).await?;
            Ok(Value::Null)
        }
        m if m == methods::scan::NAME => {
            let p: ScanParams = parse_params("scan", params)?;
            to_value(plugin.scan(p).await?)
        }
        m if m == methods::fetch_title::NAME => {
            let p: FetchTitleParams = parse_params("fetchTitle", params)?;
            to_value(plugin.fetch_title(p).await?)
        }
        m if m == methods::search_catalog::NAME => {
            let p: SearchCatalogParams = parse_params("searchCatalog", params)?;
            to_value(plugin.search_catalog(p).await?)
        }
        m if m == methods::expand_candidates::NAME => {
            let p: ExpandCandidatesParams = parse_params("expandCandidates", params)?;
            to_value(plugin.expand_candidates(p).await?)
        }
        m if m == methods::purchase_hint::NAME => {
            let p: PurchaseHintParams = parse_params("purchaseHint", params)?;
            to_value(plugin.purchase_hint(p).await?)
        }
        m if m == methods::list_deals::NAME => {
            let p: ListDealsParams = parse_params("listDeals", params)?;
            to_value(plugin.list_deals(p).await?)
        }
        m if m == methods::list_accounts::NAME => {
            let p: ListAccountsParams = parse_params("listAccounts", params)?;
            to_value(plugin.list_accounts(p).await?)
        }
        m if m == methods::catalog_detail::NAME => {
            let p: CatalogDetailParams = parse_params("catalogDetail", params)?;
            to_value(plugin.catalog_detail(p).await?)
        }
        m if m == methods::put::NAME => {
            let p: OutputPutParams = parse_params("put", params)?;
            plugin.put(p).await?;
            Ok(Value::Null)
        }
        m if m == methods::put_file::NAME => {
            let p: OutputPutFileParams = parse_params("putFile", params)?;
            plugin.put_file(p).await?;
            Ok(Value::Null)
        }
        m if m == methods::get::NAME => {
            let p: OutputGetParams = parse_params("get", params)?;
            to_value(plugin.get(p).await?)
        }
        m if m == methods::exists::NAME => {
            let p: OutputKeyParams = parse_params("exists", params)?;
            to_value(plugin.exists(p).await?)
        }
        m if m == methods::list::NAME => {
            let p: OutputListParams = parse_params("list", params)?;
            to_value(plugin.list(p).await?)
        }
        m if m == methods::probe::NAME => {
            let p: OutputKeyParams = parse_params("probe", params)?;
            to_value(plugin.probe(p).await?)
        }
        m if m == methods::copy::NAME => {
            let p: OutputCopyParams = parse_params("copy", params)?;
            plugin.copy(p).await?;
            Ok(Value::Null)
        }
        m if m == methods::delete::NAME => {
            let p: OutputKeyParams = parse_params("delete", params)?;
            plugin.delete(p).await?;
            Ok(Value::Null)
        }
        m if m == methods::touch_file::NAME => {
            let p: OutputTouchFileParams = parse_params("touchFile", params)?;
            plugin.touch_file(p).await?;
            Ok(Value::Null)
        }
        m if m == methods::db_connect::NAME => {
            let p: DbConnectParams = parse_params("dbConnect", params)?;
            to_value(plugin.db_connect(p).await?)
        }
        m if m == methods::db_ping::NAME => {
            plugin.db_ping().await?;
            Ok(Value::Null)
        }
        m if m == methods::db_query::NAME => {
            let p: StatementDto = parse_params("dbQuery", params)?;
            to_value(plugin.db_query(p).await?)
        }
        m if m == methods::db_execute::NAME => {
            let p: StatementDto = parse_params("dbExecute", params)?;
            to_value(plugin.db_execute(p).await?)
        }
        m if m == methods::db_begin::NAME => {
            let p: DbBeginParams = parse_params("dbBegin", params)?;
            to_value(plugin.db_begin(p).await?)
        }
        m if m == methods::db_commit::NAME => {
            let p: DbTxnParams = parse_params("dbCommit", params)?;
            plugin.db_commit(p).await?;
            Ok(Value::Null)
        }
        m if m == methods::db_rollback::NAME => {
            let p: DbTxnParams = parse_params("dbRollback", params)?;
            plugin.db_rollback(p).await?;
            Ok(Value::Null)
        }
        m if m == methods::db_atomic::NAME => {
            let p: DbAtomicRequest = parse_params("dbAtomic", params)?;
            to_value(plugin.db_atomic(p).await?)
        }
        other => plugin.call_raw(other, params).await,
    }
}

/// Maps a plain string into a wire [`PluginError`] with code `internal`.
///
/// Useful when adapting legacy raw handlers (see [`crate::PluginGuest`]) that
/// only produce `Err(String)` into the branded error type expected by hosts.
///
/// # Arguments
///
/// * `message` - Operator-facing explanation (no secrets).
///
/// # Returns
///
/// [`PluginError`] with [`PluginErrorCode::Internal`], empty `details`.
#[must_use]
pub fn plugin_error_from_message(message: String) -> PluginError {
    PluginError {
        code: PluginErrorCode::Internal,
        message,
        details: None,
    }
}
