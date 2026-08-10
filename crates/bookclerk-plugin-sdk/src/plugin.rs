//! Branded `BookclerkPlugin` guest trait + native Workers RPC guest runner.

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use bookclerk_plugin_abi::{
    methods, AuthenticateUserParams, CatalogDetailParams, CatalogHitDto, CliInvokeParams,
    CliInvokeResult, CliSchema, CredentialsUpdateParams, DbConnectParams, DbConnectResult,
    DiagnoseResult, EventPollResultDto, ExecResultDto, ExistsResultDto, ExpandCandidatesParams,
    ExternalUserDto, FetchTitleParams, GetResultDto, HandshakeParams, HandshakeResult,
    HealthResult, HostToPluginEvent, ListAccountsParams, ListDealsParams, LoginCompleteParams,
    LoginParams, LoginResultDto, LoginStartParams, LoginStartResultDto, ObjectInfoDto,
    ObjectProbeDto, OutputCopyParams, OutputGetParams, OutputKeyParams, OutputListParams,
    OutputPutFileParams, OutputPutParams, OutputTouchFileParams, PluginError, PluginErrorCode,
    PurchaseHintDto, PurchaseHintParams, QueryResultDto, RpcRequest, RpcResponse,
    ScanLibraryParams, ScanParams, ScanSummaryDto, SearchCatalogParams, SourceAccountDto,
    SourceFetchDto, StatementDto, SyncListeningResultDto, API_VERSION,
};

use crate::error::{Result, SdkError};
use crate::protocol::MAX_RPC_LINE_BYTES;

/// Branded guest contract — identical method surface for native and workerd SDKs.
#[async_trait]
pub trait BookclerkPlugin: Send + Sync + 'static {
    async fn handshake(
        &self,
        params: HandshakeParams,
    ) -> std::result::Result<HandshakeResult, PluginError>;

    async fn shutdown(&self) -> std::result::Result<(), PluginError> {
        Ok(())
    }

    async fn health(&self) -> std::result::Result<HealthResult, PluginError> {
        Ok(HealthResult {
            ok: true,
            ..HealthResult::default()
        })
    }

    async fn diagnose(&self) -> std::result::Result<DiagnoseResult, PluginError> {
        Ok(DiagnoseResult { lines: vec![] })
    }

    async fn start(&self) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported("start not implemented"))
    }

    async fn on_event(&self, _event: HostToPluginEvent) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported("onEvent not implemented"))
    }

    async fn poll_events(&self) -> std::result::Result<EventPollResultDto, PluginError> {
        Err(PluginError::unsupported("pollEvents not implemented"))
    }

    async fn scan_library(
        &self,
        _params: ScanLibraryParams,
    ) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported("scanLibrary not implemented"))
    }

    async fn sync_listening(&self) -> std::result::Result<SyncListeningResultDto, PluginError> {
        Err(PluginError::unsupported("syncListening not implemented"))
    }

    async fn authenticate_user(
        &self,
        _params: AuthenticateUserParams,
    ) -> std::result::Result<ExternalUserDto, PluginError> {
        Err(PluginError::unsupported("authenticateUser not implemented"))
    }

    async fn cli_describe(&self) -> std::result::Result<CliSchema, PluginError> {
        Ok(CliSchema::default())
    }

    async fn cli_invoke(
        &self,
        _params: CliInvokeParams,
    ) -> std::result::Result<CliInvokeResult, PluginError> {
        Err(PluginError::unsupported("cliInvoke not implemented"))
    }

    async fn login(
        &self,
        _params: LoginParams,
    ) -> std::result::Result<LoginResultDto, PluginError> {
        Err(PluginError::unsupported("login not implemented"))
    }

    async fn login_start(
        &self,
        _params: LoginStartParams,
    ) -> std::result::Result<LoginStartResultDto, PluginError> {
        Err(PluginError::unsupported("loginStart not implemented"))
    }

    async fn login_complete(
        &self,
        _params: LoginCompleteParams,
    ) -> std::result::Result<LoginResultDto, PluginError> {
        Err(PluginError::unsupported("loginComplete not implemented"))
    }

    async fn credentials_update(
        &self,
        _params: CredentialsUpdateParams,
    ) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported(
            "credentialsUpdate not implemented",
        ))
    }

    async fn scan(&self, _params: ScanParams) -> std::result::Result<ScanSummaryDto, PluginError> {
        Err(PluginError::unsupported("scan not implemented"))
    }

    async fn fetch_title(
        &self,
        _params: FetchTitleParams,
    ) -> std::result::Result<SourceFetchDto, PluginError> {
        Err(PluginError::unsupported("fetchTitle not implemented"))
    }

    async fn search_catalog(
        &self,
        _params: SearchCatalogParams,
    ) -> std::result::Result<Vec<CatalogHitDto>, PluginError> {
        Err(PluginError::unsupported("searchCatalog not implemented"))
    }

    async fn expand_candidates(
        &self,
        _params: ExpandCandidatesParams,
    ) -> std::result::Result<Vec<CatalogHitDto>, PluginError> {
        Err(PluginError::unsupported("expandCandidates not implemented"))
    }

    async fn purchase_hint(
        &self,
        _params: PurchaseHintParams,
    ) -> std::result::Result<Option<PurchaseHintDto>, PluginError> {
        Err(PluginError::unsupported("purchaseHint not implemented"))
    }

    async fn list_deals(
        &self,
        _params: ListDealsParams,
    ) -> std::result::Result<Vec<CatalogHitDto>, PluginError> {
        Err(PluginError::unsupported("listDeals not implemented"))
    }

    async fn list_accounts(
        &self,
        _params: ListAccountsParams,
    ) -> std::result::Result<Vec<SourceAccountDto>, PluginError> {
        Err(PluginError::unsupported("listAccounts not implemented"))
    }

    async fn catalog_detail(
        &self,
        _params: CatalogDetailParams,
    ) -> std::result::Result<Option<CatalogHitDto>, PluginError> {
        Err(PluginError::unsupported("catalogDetail not implemented"))
    }

    async fn put(&self, _params: OutputPutParams) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported("put not implemented"))
    }

    async fn put_file(&self, _params: OutputPutFileParams) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported("putFile not implemented"))
    }

    async fn get(
        &self,
        _params: OutputGetParams,
    ) -> std::result::Result<GetResultDto, PluginError> {
        Err(PluginError::unsupported("get not implemented"))
    }

    async fn exists(
        &self,
        _params: OutputKeyParams,
    ) -> std::result::Result<ExistsResultDto, PluginError> {
        Err(PluginError::unsupported("exists not implemented"))
    }

    async fn list(
        &self,
        _params: OutputListParams,
    ) -> std::result::Result<Vec<ObjectInfoDto>, PluginError> {
        Err(PluginError::unsupported("list not implemented"))
    }

    async fn probe(
        &self,
        _params: OutputKeyParams,
    ) -> std::result::Result<ObjectProbeDto, PluginError> {
        Err(PluginError::unsupported("probe not implemented"))
    }

    async fn copy(&self, _params: OutputCopyParams) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported("copy not implemented"))
    }

    async fn delete(&self, _params: OutputKeyParams) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported("delete not implemented"))
    }

    async fn touch_file(
        &self,
        _params: OutputTouchFileParams,
    ) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported("touchFile not implemented"))
    }

    async fn db_connect(
        &self,
        _params: DbConnectParams,
    ) -> std::result::Result<DbConnectResult, PluginError> {
        Err(PluginError::unsupported("dbConnect not implemented"))
    }

    async fn db_ping(&self) -> std::result::Result<(), PluginError> {
        Err(PluginError::unsupported("dbPing not implemented"))
    }

    async fn db_query(
        &self,
        _params: StatementDto,
    ) -> std::result::Result<QueryResultDto, PluginError> {
        Err(PluginError::unsupported("dbQuery not implemented"))
    }

    async fn db_execute(
        &self,
        _params: StatementDto,
    ) -> std::result::Result<ExecResultDto, PluginError> {
        Err(PluginError::unsupported("dbExecute not implemented"))
    }

    /// Escape hatch for unknown / future methods.
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
pub struct BookclerkPluginGuest;

impl BookclerkPluginGuest {
    /// Run the Workers RPC loop until stdin closes or `shutdown` succeeds.
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

fn parse_params<T: serde::de::DeserializeOwned>(
    method: &str,
    params: Value,
) -> std::result::Result<T, PluginError> {
    serde_json::from_value(params)
        .map_err(|e| PluginError::invalid_params(format!("{method} params: {e}")))
}

fn to_value<T: serde::Serialize>(value: T) -> std::result::Result<Value, PluginError> {
    serde_json::to_value(value).map_err(|e| PluginError::internal(e.to_string()))
}

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
        other => plugin.call_raw(other, params).await,
    }
}

/// Map a string error into [`PluginError`] for legacy handlers.
#[must_use]
pub fn plugin_error_from_message(message: String) -> PluginError {
    PluginError {
        code: PluginErrorCode::Internal,
        message,
        details: None,
    }
}
