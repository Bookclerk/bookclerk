//! Host-served `WEBHOOKS` binding: the instance's inbound webhooks, read like
//! a pulled queue.
//!
//! Requests arrive on the daemon's public route (`POST /hooks/<token>/<name>`,
//! see [`crate::webhook_request`]) and are stored in the library, so any node
//! can take them and any node running the instance can pull them. The guest
//! never names its instance: the binding is built for the deployed instance
//! the session was spawned for, and only for the `[[webhooks]]` rows its
//! manifest declares, when the grant carries the `webhooks` token.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bookclerk_library::control_plane::{
    ack_webhook_requests, pull_webhook_requests, webhook_url_token, PluginInstanceId,
};
use bookclerk_library::LibraryStore;
use bookclerk_plugin_abi::webhooks::{check_webhook_ack, check_webhook_name, check_webhook_pull};
use bookclerk_plugin_abi::{
    PluginError, WebhookAckRequest, WebhookPullPage, WebhookPullRequest, Webhooks,
};
use bookclerk_plugin_manifest::{PluginManifest, WebhookManifest};

use crate::consent::{grant_has_binding, PluginGrant};

/// Plugin ABI result (the wire error the guest receives).
type Result<T> = std::result::Result<T, PluginError>;

/// Grant token that authorizes every `[[webhooks]]` row of a plugin.
pub const WEBHOOKS_GRANT_BINDING: &str = "webhooks";

/// The origin (`https://host[:port]`, no trailing slash) outside services
/// reach the public webhook route on; `None` until the daemon finds one that
/// is globally reachable, or when the operator turned webhooks off.
static PUBLIC_ORIGIN: RwLock<Option<String>> = RwLock::new(None);

/// Sets the origin webhook URLs are built on.
///
/// The daemon calls this once it knows its public origin and that webhooks
/// are on (`webhooks.public = "auto"`), and with `None` to stop handing URLs
/// out. A trailing `/` is dropped.
pub fn set_webhook_public_origin(origin: Option<&str>) {
    let origin = origin
        .map(|origin| origin.trim_end_matches('/').to_string())
        .filter(|origin| !origin.is_empty());
    match PUBLIC_ORIGIN.write() {
        Ok(mut slot) => *slot = origin,
        Err(poisoned) => *poisoned.into_inner() = origin,
    }
}

/// The origin webhook URLs are built on, when one is set.
#[must_use]
pub fn webhook_public_origin() -> Option<String> {
    match PUBLIC_ORIGIN.read() {
        Ok(slot) => slot.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// What the public route needs to check a request for one webhook of an
/// instance this node runs.
#[derive(Clone)]
pub struct WebhookEndpoint {
    /// The manifest's `[[webhooks]]` row.
    pub row: WebhookManifest,
    /// The row's signing secret, when its rule names one and the operator
    /// set it.
    pub secret: Option<zeroize::Zeroizing<Vec<u8>>>,
}

impl std::fmt::Debug for WebhookEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookEndpoint")
            .field("row", &self.row)
            .field("secret", &self.secret.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

/// The `WEBHOOKS` hook of one session: the library, the deployed instance
/// and the declared rows.
#[derive(Clone)]
pub struct HostWebhooks {
    /// Library that stores tokens and deliveries.
    store: LibraryStore,
    /// Deployed instance the deliveries belong to.
    instance_id: PluginInstanceId,
    /// Declared `[[webhooks]]` rows.
    rows: Arc<Vec<WebhookManifest>>,
}

/// Builds the `WEBHOOKS` hook, or `None` when the session has no library or
/// instance, the manifest declares no `[[webhooks]]` row, or the grant lacks
/// [`WEBHOOKS_GRANT_BINDING`].
///
/// # Arguments
///
/// * `store` - The library, when the session has one.
/// * `instance_id` - The deployed instance the session runs.
/// * `manifest` - The plugin's manifest.
/// * `grant` - The covering operator grant.
#[must_use]
pub fn granted(
    store: Option<LibraryStore>,
    instance_id: Option<PluginInstanceId>,
    manifest: &PluginManifest,
    grant: &PluginGrant,
) -> Option<HostWebhooks> {
    if manifest.webhooks.is_empty() || !grant_has_binding(grant, WEBHOOKS_GRANT_BINDING) {
        return None;
    }
    Some(HostWebhooks {
        store: store?,
        instance_id: instance_id?,
        rows: Arc::new(manifest.webhooks.clone()),
    })
}

impl HostWebhooks {
    /// Checks that `name` is one of the declared rows.
    ///
    /// # Errors
    ///
    /// `invalid_params` for a malformed name; `not_found` for an undeclared
    /// one.
    fn declared(&self, name: &str) -> Result<()> {
        check_webhook_name(name)?;
        if self.rows.iter().any(|row| row.name == name) {
            Ok(())
        } else {
            Err(PluginError::not_found(format!(
                "webhook `{name}` is not declared in [[webhooks]]"
            )))
        }
    }
}

/// Maps a library failure onto the guest-visible error without its detail.
fn unavailable(err: impl std::fmt::Display) -> PluginError {
    tracing::warn!(error = %err, "webhook store failed");
    PluginError::unavailable("webhook store failed")
}

/// Current time in Unix milliseconds.
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[async_trait(?Send)]
impl Webhooks for HostWebhooks {
    async fn url(&self, name: String) -> Result<Option<String>> {
        self.declared(&name)?;
        let Some(origin) = webhook_public_origin() else {
            return Ok(None);
        };
        let token = webhook_url_token(&self.store, &self.instance_id, &name)
            .await
            .map_err(unavailable)?;
        Ok(Some(format!("{origin}/hooks/{}/{name}", token.as_str())))
    }

    async fn pull(&self, request: WebhookPullRequest) -> Result<WebhookPullPage> {
        let pull = check_webhook_pull(&request)?;
        self.declared(&request.name)?;
        pull_webhook_requests(
            &self.store,
            &self.instance_id,
            &request.name,
            pull.batch_size,
            pull.visibility_ms,
            now_ms(),
        )
        .await
        .map_err(unavailable)
    }

    async fn ack(&self, request: WebhookAckRequest) -> Result<()> {
        check_webhook_ack(&request)?;
        self.declared(&request.name)?;
        ack_webhook_requests(
            &self.store,
            &self.instance_id,
            &request.name,
            &request.acks,
            &request.retries,
        )
        .await
        .map_err(unavailable)
    }
}
