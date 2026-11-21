//! The library's public address from an ingress plugin (see
//! `docs/adr/ingress-plugins.md` and `docs/server/ingress.md`).
//!
//! [`DaemonIngress`] is what the plugin host's deployment reconciler asks
//! before it runs an `ingress` instance ([`IngressHost`]): the instance must
//! be the library's active ingress in `core.ingress`, an Owner must exist
//! and operator auth must be on. The origin the instance reports from
//! `start` is recorded in `core.ingress` and, once adopted, becomes the
//! runtime `integrations.ingress_origin` that
//! [`bookclerk_config::IntegrationsConfig::effective_public_origin`] falls
//! back to.
//!
//! [`TunnelOrigin`] serves a relay plugin's `ORIGIN` binding. Each request is
//! the `tunnel` transport: the path policy
//! ([`bookclerk_node_proto::tunnel_refusal`]) runs first, the operator's
//! credentials are refused, and the request reaches the same router as the
//! listeners with the public origin as its `Host`, the client address the
//! plugin reported as its peer and [`ListenerKind::Tunnel`] as its listener.
//! The router runs on the daemon's runtime; the bodies cross between it and
//! the plugin's thread through channels.

use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use axum::body::{Body, HttpBody};
use axum::extract::connect_info::ConnectInfo;
use axum::http::{header, HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use bookclerk_library::control_plane::{
    ingress_settings, record_started, ConfigActor, IngressExposure, IngressSettings, OriginReport,
    PluginInstanceId,
};
use bookclerk_node_proto::{normalize_tunnel_path, tunnel_refusal, TunnelExposure, TunnelRefusal};
use bookclerk_plugin_abi::origin::CheckedOriginRequest;
use bookclerk_plugin_abi::{IngressStarted, Origin, OriginHeader, OriginReply, PluginError};
use bookclerk_plugin_host::IngressHost;
use futures::StreamExt as _;
use tokio::io::AsyncRead;
use tower_service::Service as _;

use crate::api::AppState;
use crate::node_api::transport::ListenerKind;

/// Response header fields that describe the connection, which the relay
/// plugin's own server sets; never passed back to it.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Response body chunks buffered between the router and the plugin.
const BODY_CHANNEL_CHUNKS: usize = 4;

/// The actor `core.ingress` writes record for a plugin's report.
fn ingress_actor() -> ConfigActor {
    ConfigActor::Operator {
        id: "ingress".into(),
    }
}

/// What [`DaemonIngress`] and every [`TunnelOrigin`] share.
struct Shared {
    /// The daemon; weak so a running guest does not keep it alive.
    state: Weak<AppState>,
    /// The daemon's runtime, where the router runs.
    runtime: tokio::runtime::Handle,
    /// The router the listeners serve, set once they start.
    router: OnceLock<axum::Router>,
    /// `core.ingress` as last read; its exposure applies to each request.
    settings: std::sync::RwLock<IngressSettings>,
}

impl Shared {
    /// The exposure the operator chose, as last read.
    fn exposure(&self) -> TunnelExposure {
        let settings = self
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tunnel_exposure(settings.exposure)
    }

    /// Replaces the cached settings.
    fn remember(&self, settings: IngressSettings) {
        *self
            .settings
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = settings;
    }
}

/// The policy's exposure for the stored one.
fn tunnel_exposure(exposure: IngressExposure) -> TunnelExposure {
    match exposure {
        IngressExposure::Branches => TunnelExposure::Branches,
        IngressExposure::Library => TunnelExposure::Library,
    }
}

/// The daemon's side of the library's ingress ([`IngressHost`]).
pub(crate) struct DaemonIngress {
    /// State shared with the `ORIGIN` binding.
    shared: Arc<Shared>,
}

impl DaemonIngress {
    /// The ingress host for `state`, running the router on `runtime`.
    pub(crate) fn new(state: &Arc<AppState>, runtime: tokio::runtime::Handle) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Arc::downgrade(state),
                runtime,
                router: OnceLock::new(),
                settings: std::sync::RwLock::new(IngressSettings::default()),
            }),
        }
    }

    /// Hands tunnel requests the router the listeners serve. Until then a
    /// relay's requests are answered `unavailable`.
    pub(crate) fn set_router(&self, router: axum::Router) {
        let _ = self.shared.router.set(router);
    }

    /// The daemon, while it runs.
    fn state(&self) -> Result<Arc<AppState>, String> {
        self.shared
            .state
            .upgrade()
            .ok_or_else(|| "the daemon is shutting down".to_string())
    }
}

/// Why ingress may not run now, or `None` when it may: operator auth is on
/// and an Owner exists. Shared by the reconciler's admission and the
/// operator's activation.
///
/// # Errors
///
/// Returns the read error as text.
pub(crate) async fn ingress_blocked(state: &AppState) -> Result<Option<String>, String> {
    if !state.auth_snapshot().await.enabled {
        return Ok(Some(
            "operator auth is off (daemon.auth.enabled = false), so public traffic would \
             reach the library without signing in"
                .into(),
        ));
    }
    let library = state.library_snapshot().await;
    let owners = library
        .count_active_owners()
        .await
        .map_err(|err| err.to_string())?;
    Ok((owners == 0).then(|| {
        "no Owner account exists yet: claim the library on this machine first, so \
         first-run setup is never public"
            .to_string()
    }))
}

#[async_trait]
impl IngressHost for DaemonIngress {
    async fn admit(&self, plugin_instance_id: &str) -> Result<(), String> {
        let state = self.state()?;
        let library = state.library_snapshot().await;
        let settings = ingress_settings(&library)
            .await
            .map_err(|err| err.to_string())?;
        let active = settings.active();
        self.shared.remember(settings);
        let instance =
            PluginInstanceId::parse(plugin_instance_id).map_err(|err| err.to_string())?;
        if active.as_ref() != Some(&instance) {
            return Err("not the library's active ingress".into());
        }
        match ingress_blocked(&state).await? {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    }

    fn origin(&self) -> Arc<dyn Origin + Send + Sync> {
        Arc::new(TunnelOrigin {
            shared: Arc::clone(&self.shared),
        })
    }

    async fn started(
        &self,
        plugin_instance_id: &str,
        started: IngressStarted,
    ) -> Result<(), String> {
        let state = self.state()?;
        let instance =
            PluginInstanceId::parse(plugin_instance_id).map_err(|err| err.to_string())?;
        let library = state.library_snapshot().await;
        let report = record_started(&library, &ingress_actor(), &instance, &started)
            .await
            .map_err(|err| err.to_string())?;
        let serving = match report {
            OriginReport::Adopted(origin) => {
                tracing::info!(
                    plugin_instance_id,
                    origin = origin.as_str(),
                    "ingress is serving"
                );
                origin.as_str().to_string()
            }
            OriginReport::Pending { adopted, reported } => {
                tracing::warn!(
                    plugin_instance_id,
                    adopted = adopted.as_str(),
                    reported = reported.as_str(),
                    "the ingress reported a new public origin; the library keeps its name until \
                     the operator accepts it (bookclerk ingress accept-origin)"
                );
                adopted
            }
            OriginReport::NotActive => return Err("not the library's active ingress".into()),
        };
        if let Ok(settings) = ingress_settings(&library).await {
            self.shared.remember(settings);
        }
        state.config.write().await.integrations.ingress_origin = Some(serving);
        Ok(())
    }

    async fn stopped(&self, plugin_instance_id: &str) {
        tracing::info!(plugin_instance_id, "ingress stopped serving");
        if let Some(state) = self.shared.state.upgrade() {
            state.config.write().await.integrations.ingress_origin = None;
        }
    }
}

/// The library as a relay plugin reaches it: the `ORIGIN` binding.
pub(crate) struct TunnelOrigin {
    /// State shared with [`DaemonIngress`].
    shared: Arc<Shared>,
}

/// One answer before its body crosses to the plugin.
struct TunnelAnswer {
    /// HTTP status.
    status: u16,
    /// Header fields the plugin may pass on.
    headers: Vec<OriginHeader>,
    /// The body, already bridged off the daemon's runtime.
    body: Option<Pin<Box<dyn AsyncRead + Send>>>,
}

#[async_trait(?Send)]
impl Origin for TunnelOrigin {
    async fn fetch(
        &self,
        request: CheckedOriginRequest,
        body: Option<Pin<Box<dyn AsyncRead + Send>>>,
    ) -> bookclerk_plugin_abi::Result<OriginReply> {
        let client = tunnel_client(request.client)?;
        let shared = Arc::clone(&self.shared);
        let answer = self
            .shared
            .runtime
            .spawn(async move { serve(&shared, request, client, body).await })
            .await
            .map_err(|err| {
                PluginError::unavailable(format!("the library dropped the request: {err}"))
            })??;
        Ok(OriginReply {
            status: answer.status,
            headers: answer.headers,
            body: answer.body,
        })
    }
}

/// The client address a tunnel request may carry.
///
/// # Errors
///
/// [`PluginError::invalid_params`] when the plugin reported none, or a
/// loopback or unspecified address: a tunnel request is never local.
fn tunnel_client(client: Option<IpAddr>) -> bookclerk_plugin_abi::Result<IpAddr> {
    let Some(client) = client.map(|ip| ip.to_canonical()) else {
        return Err(PluginError::invalid_params(
            "a tunnel request needs the client address",
        ));
    };
    if client.is_loopback() || client.is_unspecified() {
        return Err(PluginError::invalid_params(
            "a tunnel request's client address may not be loopback or unspecified",
        ));
    }
    Ok(client)
}

/// Serves one tunnel request on the daemon's runtime.
///
/// # Errors
///
/// `unavailable` while the daemon has no router, public origin or auth;
/// `invalid_params` for a head the router could not take.
async fn serve(
    shared: &Shared,
    request: CheckedOriginRequest,
    client: IpAddr,
    body: Option<Pin<Box<dyn AsyncRead + Send>>>,
) -> bookclerk_plugin_abi::Result<TunnelAnswer> {
    let Some(state) = shared.state.upgrade() else {
        return Err(PluginError::unavailable("the daemon is shutting down"));
    };
    let Some(router) = shared.router.get().cloned() else {
        return Err(PluginError::unavailable("the library is still starting"));
    };
    let path = normalize_tunnel_path(request.target.split('?').next().unwrap_or(""));
    if let Some(refusal) = tunnel_refusal(&path, shared.exposure()) {
        return Ok(refused(refusal));
    }
    let auth = state.auth_snapshot().await;
    if !auth.enabled {
        return Err(PluginError::unavailable("operator auth is off"));
    }
    if carries_operator_credential(&request.headers, |token| auth.token_matches(token)) {
        return Ok(refused(TunnelRefusal::OperatorOnly));
    }
    let Some(authority) = public_authority(
        state
            .config
            .read()
            .await
            .integrations
            .effective_public_origin(),
    ) else {
        return Err(PluginError::unavailable(
            "the library has no public origin yet",
        ));
    };
    let http = tunnel_request(request, &authority, client, body)?;
    let mut router = router;
    let response = match router.call(http).await {
        Ok(response) => response,
        Err(never) => match never {},
    };
    Ok(answer(&shared.runtime, response))
}

/// The `Host` value for `origin`: its host and any explicit port.
fn public_authority(origin: Option<&str>) -> Option<String> {
    let url = url::Url::parse(origin?.trim()).ok()?;
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

/// True when the request carries the operator token as its bearer, or an
/// operator session cookie: the operator signs in on this machine only.
fn carries_operator_credential(
    headers: &[OriginHeader],
    token_matches: impl Fn(&str) -> bool,
) -> bool {
    headers.iter().any(|field| {
        if field.name.eq_ignore_ascii_case("authorization") {
            let value = field.value.trim();
            let token = value
                .get(..7)
                .filter(|scheme| scheme.eq_ignore_ascii_case("bearer "))
                .map(|_| value[7..].trim());
            return token.is_some_and(|token| !token.is_empty() && token_matches(token));
        }
        field.name.eq_ignore_ascii_case("cookie")
            && field.value.split(';').any(|pair| {
                pair.trim()
                    .split_once('=')
                    .is_some_and(|(name, _)| name.trim() == crate::auth::SESSION_COOKIE)
            })
    })
}

/// True for forwarding fields a proxy would add; the tunnel's client comes
/// from the plugin's report, never a header.
fn is_forwarding_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "via" || name == "forwarded" || name == "x-real-ip" || name.starts_with("x-forwarded-")
}

/// Builds the router's request for one tunnel request.
///
/// # Errors
///
/// [`PluginError::invalid_params`] when the method, target or a header is
/// one the HTTP types refuse.
fn tunnel_request(
    request: CheckedOriginRequest,
    authority: &str,
    client: IpAddr,
    body: Option<Pin<Box<dyn AsyncRead + Send>>>,
) -> bookclerk_plugin_abi::Result<Request<Body>> {
    let method = Method::from_bytes(request.method.as_bytes())
        .map_err(|_| PluginError::invalid_params("the request method is not an HTTP token"))?;
    let body = match body {
        Some(reader) => Body::from_stream(tokio_util::io::ReaderStream::new(reader)),
        None => Body::empty(),
    };
    let mut http = Request::builder()
        .method(method)
        .uri(request.target.as_str())
        .body(body)
        .map_err(|_| PluginError::invalid_params("the request target is not a path and query"))?;
    let headers = http.headers_mut();
    for field in &request.headers {
        if is_forwarding_header(&field.name) {
            continue;
        }
        let name = HeaderName::from_bytes(field.name.as_bytes())
            .map_err(|_| PluginError::invalid_params("a request header name is not a token"))?;
        let value = HeaderValue::from_str(&field.value)
            .map_err(|_| PluginError::invalid_params("a request header value is not text"))?;
        headers.append(name, value);
    }
    let host = HeaderValue::from_str(authority)
        .map_err(|_| PluginError::unavailable("the public origin is not a host name"))?;
    headers.insert(header::HOST, host);
    let extensions = http.extensions_mut();
    extensions.insert(ConnectInfo(SocketAddr::new(client, 0)));
    extensions.insert(ListenerKind::Tunnel);
    Ok(http)
}

/// The answer for a path or credential the tunnel refuses.
fn refused(refusal: TunnelRefusal) -> TunnelAnswer {
    let message = match refusal {
        TunnelRefusal::OperatorOnly => "This is only available on the machine running the library.",
        TunnelRefusal::NotExposed => "The library does not serve this over its public address.",
    };
    let status = StatusCode::FORBIDDEN;
    let body = serde_json::json!({
        "error": refusal.as_str(),
        "message": message,
        "status": status.as_u16(),
    })
    .to_string()
    .into_bytes();
    TunnelAnswer {
        status: status.as_u16(),
        headers: vec![OriginHeader {
            name: "content-type".into(),
            value: "application/json".into(),
        }],
        body: Some(Box::pin(std::io::Cursor::new(body))),
    }
}

/// The router's response as the plugin receives it: connection fields and
/// values that are not visible ASCII are dropped, and the body is pumped on
/// `runtime` into a channel the plugin reads.
fn answer(runtime: &tokio::runtime::Handle, response: Response<Body>) -> TunnelAnswer {
    let status = response.status().as_u16();
    let mut headers = Vec::with_capacity(response.headers().len());
    for (name, value) in response.headers() {
        if HOP_BY_HOP.contains(&name.as_str()) {
            continue;
        }
        match value.to_str() {
            Ok(text)
                if text
                    .bytes()
                    .all(|b| b == b'\t' || (0x20..0x7f).contains(&b)) =>
            {
                headers.push(OriginHeader {
                    name: name.as_str().to_string(),
                    value: text.to_string(),
                });
            }
            _ => tracing::debug!(
                header = name.as_str(),
                "tunnel response header not text; dropped"
            ),
        }
    }
    let body = response.into_body();
    let body = if body.is_end_stream() {
        None
    } else {
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<std::io::Result<axum::body::Bytes>>(BODY_CHANNEL_CHUNKS);
        runtime.spawn(async move {
            let mut stream = body.into_data_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(std::io::Error::other);
                let failed = chunk.is_err();
                if tx.send(chunk).await.is_err() || failed {
                    return;
                }
            }
        });
        let stream = futures::stream::poll_fn(move |cx| rx.poll_recv(cx));
        let reader: Pin<Box<dyn AsyncRead + Send>> =
            Box::pin(tokio_util::io::StreamReader::new(stream));
        Some(reader)
    };
    TunnelAnswer {
        status,
        headers,
        body,
    }
}

/// `GET /api/config/ingress` and the answer to `POST /api/config/ingress`: the library's
/// ingress settings and what they mean now.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub(crate) struct IngressStatus {
    /// The plugin instance serving the library's public address, as its
    /// UUID; `null` when ingress is off.
    #[schema(example = "3f9c2b7e-1d4a-4c8e-9b2f-6a7d8e9f0a1b")]
    pub(crate) active_instance: Option<String>,
    /// The active instance's plugin, when the instance exists.
    #[schema(example = "cloudflare-relay")]
    pub(crate) plugin_key: Option<String>,
    /// What public traffic may reach.
    pub(crate) exposure: IngressExposure,
    /// The adopted public origin, kept while ingress is off.
    #[schema(example = "https://relay.example.workers.dev")]
    pub(crate) origin: Option<String>,
    /// A different origin the active instance reported, waiting for the
    /// operator to accept it.
    #[schema(example = "https://library.example.net")]
    pub(crate) reported_origin: Option<String>,
    /// The origin the library serves under now: `integrations.public_origin`
    /// when set, else the ingress origin while the instance runs.
    #[schema(example = "https://relay.example.workers.dev")]
    pub(crate) public_origin: Option<String>,
    /// True when `integrations.public_origin` in `config.toml` overrides the
    /// ingress origin.
    #[schema(example = false)]
    pub(crate) operator_override: bool,
    /// Why ingress can't run now (no Owner yet, or operator auth off);
    /// `null` when it can.
    #[schema(example = json!(null))]
    pub(crate) blocked: Option<String>,
}

/// `POST /api/config/ingress` body: each field is one change, applied in order
/// (off or on, then exposure, then accepting a reported origin).
#[derive(Debug, Default, serde::Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct IngressChange {
    /// Makes this the library's active ingress: a plugin instance UUID, or a
    /// plugin key for that plugin's default instance.
    #[serde(default)]
    #[schema(example = "cloudflare-relay")]
    pub(crate) activate: Option<String>,
    /// Turns ingress off; the adopted origin is kept.
    #[serde(default)]
    #[schema(example = false)]
    pub(crate) deactivate: bool,
    /// What public traffic may reach.
    #[serde(default)]
    pub(crate) exposure: Option<IngressExposure>,
    /// Adopts the origin the active instance reported. Renaming the library
    /// breaks passkeys and sign-in redirects registered under the old name.
    #[serde(default)]
    #[schema(example = false)]
    pub(crate) accept_reported_origin: bool,
}

/// An error answer with its status.
fn ingress_error(status: StatusCode, message: impl std::fmt::Display) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    (
        status,
        axum::Json(crate::http_error::ErrorMessage::new(message)),
    )
        .into_response()
}

/// The status the operator sees for `settings`.
async fn status_of(
    state: &AppState,
    settings: IngressSettings,
) -> Result<IngressStatus, axum::response::Response> {
    let library = state.library_snapshot().await;
    let plugin_key = match settings.active() {
        Some(id) => bookclerk_library::control_plane::load_plugin_instance(&library, &id)
            .await
            .map_err(crate::config_authority::events_error)?
            .map(|instance| instance.plugin_key),
        None => None,
    };
    let blocked = ingress_blocked(state)
        .await
        .map_err(|err| ingress_error(StatusCode::INTERNAL_SERVER_ERROR, err))?;
    let cfg = state.config.read().await;
    let operator_override = cfg
        .integrations
        .public_origin
        .as_deref()
        .is_some_and(|origin| !origin.trim().is_empty());
    Ok(IngressStatus {
        active_instance: settings.active().map(|id| id.to_string()),
        plugin_key,
        exposure: settings.exposure,
        origin: settings.origin,
        reported_origin: settings.reported_origin,
        public_origin: cfg
            .integrations
            .effective_public_origin()
            .map(str::to_owned),
        operator_override,
        blocked,
    })
}

/// `GET /api/config/ingress`: the library's public address settings.
#[utoipa::path(
    get,
    path = "/api/config/ingress",
    responses(
        (status = 200, description = "The ingress settings.", body = IngressStatus),
        (status = 500, description = "The library could not be read.", body = crate::http_error::ErrorMessage, example = json!({"error": "ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection"})),
    ),
)]
pub(crate) async fn get_ingress(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Result<axum::Json<IngressStatus>, axum::response::Response> {
    let library = state.library_snapshot().await;
    let settings = ingress_settings(&library)
        .await
        .map_err(crate::config_authority::events_error)?;
    Ok(axum::Json(status_of(&state, settings).await?))
}

/// `POST /api/config/ingress`: turns ingress on or off, sets what it exposes, or
/// accepts a reported origin. The deployment reconciler then starts or
/// stops the instance.
#[utoipa::path(
    post,
    path = "/api/config/ingress",
    request_body = IngressChange,
    responses(
        (status = 200, description = "The settings after the change.", body = IngressStatus),
        (status = 400, description = "`activate` and `deactivate` were both set, or `activate` is neither an instance id nor a plugin key with an instance.", body = crate::http_error::ErrorMessage, example = json!({"error": "no plugin instance for `cloudflare-relay`"})),
        (status = 409, description = "Ingress can't be turned on yet: no Owner exists, or operator auth is off.", body = crate::http_error::ErrorMessage, example = json!({"error": "no Owner account exists yet: claim the library on this machine first, so first-run setup is never public"})),
        (status = 500, description = "The library could not be read or written.", body = crate::http_error::ErrorMessage, example = json!({"error": "ORM / database plugin error: Connection Error: pool timed out while waiting for an open connection"})),
    ),
)]
pub(crate) async fn post_ingress(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::Json(change): axum::Json<IngressChange>,
) -> Result<axum::Json<IngressStatus>, axum::response::Response> {
    use bookclerk_library::control_plane::{
        accept_reported_origin, activate_ingress, deactivate_ingress, default_instance_for_key,
        load_plugin_instance, set_ingress_exposure,
    };
    if change.activate.is_some() && change.deactivate {
        return Err(ingress_error(
            StatusCode::BAD_REQUEST,
            "`activate` and `deactivate` can't both be set",
        ));
    }
    let library = state.library_snapshot().await;
    let actor = ConfigActor::Operator {
        id: "operator".into(),
    };
    let fail = crate::config_authority::events_error;
    let mut settings = None;
    if let Some(target) = change.activate.as_deref().map(str::trim) {
        let instance = match PluginInstanceId::parse(target) {
            Ok(id) => load_plugin_instance(&library, &id).await.map_err(fail)?,
            Err(_) => default_instance_for_key(&library, target)
                .await
                .map_err(fail)?,
        }
        .ok_or_else(|| {
            ingress_error(
                StatusCode::BAD_REQUEST,
                format!("no plugin instance for `{target}`"),
            )
        })?;
        if let Some(reason) = ingress_blocked(&state)
            .await
            .map_err(|err| ingress_error(StatusCode::INTERNAL_SERVER_ERROR, err))?
        {
            return Err(ingress_error(StatusCode::CONFLICT, reason));
        }
        settings = Some(
            activate_ingress(&library, &actor, &instance.id)
                .await
                .map_err(fail)?
                .body,
        );
    }
    if change.deactivate {
        settings = Some(
            deactivate_ingress(&library, &actor)
                .await
                .map_err(fail)?
                .body,
        );
    }
    if let Some(exposure) = change.exposure {
        settings = Some(
            set_ingress_exposure(&library, &actor, exposure)
                .await
                .map_err(fail)?
                .body,
        );
    }
    if change.accept_reported_origin {
        let accepted = accept_reported_origin(&library, &actor)
            .await
            .map_err(fail)?
            .body;
        if let Some(origin) = accepted.serving_origin() {
            let mut cfg = state.config.write().await;
            if cfg.integrations.ingress_origin.is_some() {
                cfg.integrations.ingress_origin = Some(origin.to_string());
            }
        }
        settings = Some(accepted);
    }
    let settings = match settings {
        Some(settings) => {
            // Start, stop or re-admit the instance now rather than at the
            // next pass; a slow guest start must not hold the request.
            let reconcile = Arc::clone(&state);
            tokio::spawn(async move {
                if let Err(err) = crate::config_authority::reconcile_deployments(&reconcile).await {
                    tracing::warn!(error = %err, "deployment reconcile after an ingress change failed");
                }
            });
            settings
        }
        None => ingress_settings(&library).await.map_err(fail)?,
    };
    Ok(axum::Json(status_of(&state, settings).await?))
}

#[cfg(test)]
#[path = "ingress_tests.rs"]
mod tests;
