//! The public webhook route, `POST /hooks/<token>/<name>`, and the origin
//! webhook URLs are built on.
//!
//! The route is outside operator auth, CSRF and the session layers: the
//! unguessable token in the path is the only credential, and a row with an
//! `hmac-sha256` rule also needs a valid signature. The request checks run
//! in `bookclerk_workerd::webhook_request` (re-exported by
//! `bookclerk_plugin_host`), sans-IO, so the fuzzer and
//! the unit tests drive the same code. A stored request waits in the
//! library until the instance's guest pulls it through its `WEBHOOKS`
//! binding.
//!
//! The route answers on a node that runs the instance; elsewhere it answers
//! 503 so the sender retries.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bookclerk_config::{Config, WebhooksExposure};
use bookclerk_library::control_plane::{
    find_webhook, store_webhook_request, WebhookRequest, WebhookStoreOutcome, WebhookUrlToken,
};
use bookclerk_plugin_abi::webhooks::is_valid_webhook_name;
use bookclerk_plugin_abi::WebhookDelivered;
use bookclerk_plugin_host::webhook_request::{
    check_webhook_head, verify_webhook_request, WebhookRateLimiter, WebhookRefusal,
};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::api::AppState;

/// How often the published webhook origin is refreshed from the config, so
/// a change to `integrations.public_origin` or `plugins.webhooks` reaches
/// the `WEBHOOKS` binding without a restart.
const ORIGIN_REFRESH: Duration = Duration::from_secs(30);

/// Per-instance request rate limits of this node.
static RATE_LIMITER: Mutex<Option<WebhookRateLimiter>> = Mutex::new(None);

/// The origin webhook URLs are built on, or `None` when none is handed out.
///
/// A URL is only useful when an outside service can reach it, so the
/// origin must be the configured `integrations.public_origin`, use `https`,
/// and name a host that is globally reachable: a public DNS name or a
/// global IP address, not `localhost`, a private, loopback or link-local
/// address, or a name under a reserved local suffix.
///
/// # Arguments
///
/// * `public_origin` - `integrations.public_origin`, when set.
/// * `exposure` - `plugins.webhooks`.
#[must_use]
pub fn webhook_origin(public_origin: Option<&str>, exposure: WebhooksExposure) -> Option<String> {
    match exposure {
        WebhooksExposure::Off => return None,
        WebhooksExposure::Auto => {}
    }
    let origin = crate::origin::configured_public_origin(public_origin)?;
    let url = url::Url::parse(&origin).ok()?;
    if url.scheme() != "https"
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let reachable = match url.host()? {
        url::Host::Domain(name) => is_public_name(name),
        url::Host::Ipv4(addr) => is_global_v4(addr),
        url::Host::Ipv6(addr) => is_global_v6(addr),
    };
    reachable.then(|| url.origin().ascii_serialization())
}

/// Suffixes of names that resolve only on a local network or not at all.
const LOCAL_SUFFIXES: &[&str] = &[
    "localhost",
    "local",
    "internal",
    "lan",
    "home.arpa",
    "test",
    "invalid",
    "example",
    "intranet",
    "corp",
    "home",
];

/// True for a DNS name that can be public: dotted, and not under a local
/// or reserved suffix.
fn is_public_name(name: &str) -> bool {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    if !name.contains('.') {
        return false;
    }
    !LOCAL_SUFFIXES
        .iter()
        .any(|suffix| name == *suffix || name.ends_with(&format!(".{suffix}")))
}

/// True for an IPv4 address reachable from the Internet.
fn is_global_v4(addr: Ipv4Addr) -> bool {
    let [a, b, c, _] = addr.octets();
    !(addr.is_private()
        || addr.is_loopback()
        || addr.is_link_local()
        || addr.is_unspecified()
        || addr.is_broadcast()
        || addr.is_documentation()
        || addr.is_multicast()
        // Shared address space (100.64.0.0/10), carrier-grade NAT.
        || (a == 100 && (64..128).contains(&b))
        // Benchmarking (198.18.0.0/15).
        || (a == 198 && (b == 18 || b == 19))
        // IETF protocol assignments (192.0.0.0/24) and reserved 240/4.
        || (a == 192 && b == 0 && c == 0)
        || a >= 240
        || a == 0)
}

/// True for an IPv6 address reachable from the Internet.
fn is_global_v6(addr: Ipv6Addr) -> bool {
    if let Some(v4) = addr.to_ipv4_mapped() {
        return is_global_v4(v4);
    }
    let first = addr.segments()[0];
    !(addr.is_loopback()
        || addr.is_unspecified()
        || addr.is_multicast()
        // Unique local fc00::/7.
        || (first & 0xfe00) == 0xfc00
        // Link-local fe80::/10.
        || (first & 0xffc0) == 0xfe80
        // Documentation 2001:db8::/32.
        || (first == 0x2001 && addr.segments()[1] == 0x0db8))
}

/// Publishes the webhook origin from `config` to the `WEBHOOKS` binding.
pub fn publish_origin(config: &Config) {
    let origin = webhook_origin(
        config.integrations.public_origin.as_deref(),
        config.plugins.webhooks,
    );
    bookclerk_plugin_host::set_webhook_public_origin(origin.as_deref());
}

/// Publishes the webhook origin now and keeps it current.
pub fn spawn_origin_publisher(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            publish_origin(&*state.config.read().await);
            tokio::time::sleep(ORIGIN_REFRESH).await;
        }
    });
}

/// Routes of the public webhook endpoint.
pub(crate) fn webhook_routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(post_hook))
}

/// The router serving [`webhook_routes`], with the API deadline.
pub fn webhook_router<S>(state: Arc<AppState>) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let (routes, _) = webhook_routes().split_for_parts();
    routes
        .layer(axum::middleware::from_fn(
            crate::api::api_timeout_middleware,
        ))
        .with_state(state)
}

/// Answers a refusal with its bare status.
fn refuse(refusal: WebhookRefusal) -> Response {
    StatusCode::from_u16(refusal.status())
        .unwrap_or(StatusCode::SERVICE_UNAVAILABLE)
        .into_response()
}

/// Current time in Unix milliseconds.
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Takes one request from the bucket of `(instance, name)`.
fn take_rate(instance: &str, name: &str, rate: u32, now_ms: i64) -> Result<(), WebhookRefusal> {
    let mut slot = RATE_LIMITER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    slot.get_or_insert_with(WebhookRateLimiter::default)
        .take(instance, name, rate, now_ms)
}

/// Stores one inbound webhook request for a plugin instance.
///
/// The token is the credential; a row with a signing rule also needs a
/// valid signature. The reply body never says more than the status.
#[utoipa::path(
    post,
    path = "/hooks/{token}/{name}",
    params(
        ("token" = String, Path, description = "The webhook's URL token: 43 characters of base64url, from the plugin's `WEBHOOKS.url`."),
        ("name" = String, Path, description = "The `[[webhooks]]` row's name."),
    ),
    request_body(content = crate::openapi::BinaryBody, description = "The request body, kept as sent, at most the row's `max_body_bytes`.", content_type = "*/*"),
    responses(
        (status = 204, description = "Stored; the plugin reads it on its next pull."),
        (status = 400, description = "A kept header repeats, is too long or holds a control character."),
        (status = 401, description = "The signature is missing or does not match."),
        (status = 404, description = "The token or name matches no webhook."),
        (status = 413, description = "The body is larger than the row allows."),
        (status = 429, description = "The webhook is over its `rate_per_minute`."),
        (status = 503, description = "The webhook cannot take requests now: webhooks are off, the signing secret is not set, this node does not run the instance, the library is unavailable, or the instance already holds its cap of waiting requests. Retry later."),
    ),
)]
pub async fn post_hook(
    State(state): State<Arc<AppState>>,
    Path((token, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    match accept(&state, &token, &name, &headers, body).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(refusal) => refuse(refusal),
    }
}

/// Runs the route's checks in order and stores the request.
async fn accept(
    state: &Arc<AppState>,
    token: &str,
    name: &str,
    headers: &HeaderMap,
    body: Body,
) -> Result<(), WebhookRefusal> {
    match state.config.read().await.plugins.webhooks {
        WebhooksExposure::Off => return Err(WebhookRefusal::Unavailable),
        WebhooksExposure::Auto => {}
    }
    let token = WebhookUrlToken::parse(token).ok_or(WebhookRefusal::NotFound)?;
    if !is_valid_webhook_name(name) {
        return Err(WebhookRefusal::NotFound);
    }
    let library = state.library_snapshot().await;
    let target = find_webhook(&library, &token)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "webhook token lookup failed");
            WebhookRefusal::Unavailable
        })?
        .filter(|target| target.name == name)
        .ok_or(WebhookRefusal::NotFound)?;
    let instance = target.instance_id.as_str().to_string();
    let endpoint = state
        .deployment_runtime()
        .webhook_endpoint(&instance, name)
        .ok_or(WebhookRefusal::Unavailable)?;
    let row = &endpoint.row;
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    check_webhook_head(row, "POST", declared)?;
    let now = now_ms();
    take_rate(&instance, name, row.rate_per_minute, now)?;
    let limit = usize::try_from(row.max_body_bytes).unwrap_or(usize::MAX);
    let body = axum::body::to_bytes(body, limit)
        .await
        .map_err(|_| WebhookRefusal::PayloadTooLarge)?;
    let pairs: Vec<(String, Vec<u8>)> = headers
        .iter()
        .map(|(key, value)| (key.as_str().to_string(), value.as_bytes().to_vec()))
        .collect();
    let secret = endpoint.secret.as_ref().map(|secret| secret.as_slice());
    let kept = verify_webhook_request(row, secret, &pairs, &body)?;
    let request = WebhookRequest {
        headers: kept,
        body: body.to_vec(),
        received_at_ms: now,
        retention_ms: i64::from(row.retention_secs).saturating_mul(1000),
    };
    let outcome = store_webhook_request(&library, &target, &request)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "webhook request not stored");
            WebhookRefusal::Unavailable
        })?;
    match outcome {
        WebhookStoreOutcome::Stored { waiting } => {
            state.deployment_runtime().wake_webhook(
                &instance,
                WebhookDelivered {
                    name: name.to_string(),
                    waiting,
                },
            );
            crate::destination_sync::nudge_destination_sync();
            Ok(())
        }
        WebhookStoreOutcome::Full => Err(WebhookRefusal::Unavailable),
    }
}

#[cfg(test)]
#[path = "webhooks_tests.rs"]
mod tests;
