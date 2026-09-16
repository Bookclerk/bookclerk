//! [`Integration`] adapter over an external plugin process.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bookclerk_config::Config;
use bookclerk_integrations::{
    Brand, EventSubscription, ExternalUser, Integration, IntegrationContext, IntegrationEvent,
    IntegrationHealth, IntegrationRegistry, ProvidedOidcClient,
};
use bookclerk_plugin_sdk::{
    AuthenticateUserParams, DomainEvent, EventResult, ExtensibleConfig,
    IntegrationContext as AbiIntegrationContext, ScanLibraryParams, PRODUCT_API_VERSION,
};
use serde_json::Value;
use tracing::warn;

use crate::discover::DiscoveredPlugin;
use crate::rpc_session::{PluginSession, HOST_SHARED_ACCOUNT};
use crate::Result;

/// External integration backed by a discovered plugin binary.
pub struct ExternalIntegration {
    /// Cap'n Proto session (never given `library.db`).
    session: Arc<PluginSession>,
    /// Typed factory context (granted plugin config table as JSON payload).
    ctx: AbiIntegrationContext,
    /// Operator-facing name from describe metadata (falls back to the manifest id).
    display_name: String,
    /// Whether this integration is enabled in host config after describe.
    enabled: bool,
    /// Portal brand colors/icon leaked from describe metadata, if the guest supplied one.
    brand: Option<Brand>,
    /// When true, the host may call the guest credential-login RPC (username/password).
    allow_credential_login: bool,
    /// Durable outbox subscriptions from `plugin.toml`.
    event_subscriptions: Vec<EventSubscription>,
    /// Cancels the host-side `event_poll` loop from [`Self::start`].
    poll_cancel: Arc<AtomicBool>,
    /// Bumped on each [`Self::start`]/[`Self::stop`] so a superseded poll loop exits.
    poll_epoch: Arc<AtomicU64>,
}

impl ExternalIntegration {
    /// Spawn and describe an integration plugin.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn spawn(plugin: &DiscoveredPlugin, config: &Config) -> Result<Self> {
        if plugin.manifest.api_version != PRODUCT_API_VERSION {
            return Err(crate::PluginError::message(format!(
                "plugin `{}` api_version {} is not supported",
                plugin.manifest.id, plugin.manifest.api_version
            )));
        }
        let table = crate::settings_table(config, plugin);
        let config_json = Value::Object(
            table
                .iter()
                .map(|(k, v)| (k.clone(), toml_to_json(v)))
                .collect(),
        );
        let allow_credential_login = table
            .get("allow_credential_login")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let session = Arc::new(
            PluginSession::spawn_for_account(
                plugin,
                config,
                config_json.clone(),
                HOST_SHARED_ACCOUNT,
            )
            .await?,
        );
        let source_config = crate::spawn_config_for_grant(session.grant(), config_json);
        let describe = session.describe_snapshot();
        let display_name = describe
            .display_name
            .clone()
            .or_else(|| plugin.manifest.name.clone())
            .unwrap_or_else(|| plugin.manifest.id.clone());
        let brand = brand_from_abi(describe.brand.as_ref());
        let event_subscriptions = plugin
            .manifest
            .capabilities
            .events
            .subscriptions
            .iter()
            .map(|s| EventSubscription {
                event_type: s.event_type.clone(),
                schema_versions: s.schema_versions.clone(),
                supports_suspend: s.supports_suspend,
                resource_class: if s.resource_class.trim().is_empty() {
                    "network".into()
                } else {
                    s.resource_class.clone()
                },
                filter: s.filter.clone().filter(|v| !v.is_null()),
            })
            .collect();
        Ok(Self {
            session,
            ctx: AbiIntegrationContext {
                config: ExtensibleConfig::json(&source_config),
            },
            display_name,
            enabled: true,
            brand,
            allow_credential_login,
            event_subscriptions,
            poll_cancel: Arc::new(AtomicBool::new(false)),
            poll_epoch: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Runs one typed integration method through the plugin session.
    ///
    /// # Errors
    ///
    /// Returns when the factory or the guest method fails.
    async fn int_call<T, F, Fut>(&self, call: F) -> bookclerk_integrations::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Box<dyn bookclerk_plugin_sdk::Integration>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = std::result::Result<T, bookclerk_plugin_sdk::PluginError>>
            + 'static,
    {
        Ok(self.session.integration(self.ctx.clone(), call).await?)
    }
}

/// Discover and register external integration plugins.
///
/// Duplicate `(kind, id)` claims among discovered manifests are a hard error
/// (from [`crate::discover_plugins`]). When an external id is already registered
/// in-process (dual-load `register()` path), the external copy is skipped.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn load_external_integrations(
    config: &Config,
    registry: &mut IntegrationRegistry,
) -> Result<()> {
    for plugin in crate::discover_plugins(config)? {
        if plugin.manifest.kind != crate::PluginKind::Integration {
            continue;
        }
        if !config.integrations.is_enabled(&plugin.manifest.id) {
            continue;
        }
        if registry.get(&plugin.manifest.id).is_some() {
            tracing::debug!(
                id = %plugin.manifest.id,
                path = %plugin.root.join("plugin.toml").display(),
                "skipping external integration — already registered in-process"
            );
            continue;
        }
        match ExternalIntegration::spawn(&plugin, config).await {
            Ok(i) => {
                tracing::info!(id = %plugin.manifest.id, "loaded external integration plugin");
                registry.register(Arc::new(i));
            }
            Err(err) => {
                tracing::warn!(
                    id = %plugin.manifest.id,
                    %err,
                    "skipping external integration plugin"
                );
            }
        }
    }
    Ok(())
}

#[async_trait]
impl Integration for ExternalIntegration {
    fn id(&self) -> &str {
        self.session.id()
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    async fn start(&self, ctx: IntegrationContext) -> bookclerk_integrations::Result<()> {
        if self.session.has_capability("start") {
            let _ = self
                .int_call(|stub| async move { stub.start().await })
                .await;
        }
        // Host polls `event_poll` and kicks off core workflows (e.g. claim tickets).
        // The plugin remains oblivious to what the host does with the signal.
        if self.session.has_capability("pollEvents") {
            if let Some(on_user) = ctx.on_external_user {
                let epoch = self.poll_epoch.fetch_add(1, Ordering::SeqCst) + 1;
                self.poll_cancel.store(false, Ordering::SeqCst);
                let session = self.session.clone();
                let ctx = self.ctx.clone();
                let plugin_id = self.id().to_string();
                let cancel = self.poll_cancel.clone();
                let epoch_flag = self.poll_epoch.clone();
                tokio::spawn(async move {
                    loop {
                        if cancel.load(Ordering::SeqCst)
                            || epoch_flag.load(Ordering::SeqCst) != epoch
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        if cancel.load(Ordering::SeqCst)
                            || epoch_flag.load(Ordering::SeqCst) != epoch
                        {
                            break;
                        }
                        match session
                            .integration(
                                ctx.clone(),
                                |stub| async move { stub.poll_events().await },
                            )
                            .await
                        {
                            Ok(users) => {
                                for user in users {
                                    on_user(ExternalUser {
                                        provider: if user.provider.is_empty() {
                                            plugin_id.clone()
                                        } else {
                                            user.provider
                                        },
                                        external_user_id: user.external_user_id,
                                        display_name: user.display_name,
                                        access_token: None,
                                    });
                                }
                            }
                            Err(err) => {
                                warn!(id = %plugin_id, %err, "integration event_poll failed");
                            }
                        }
                    }
                });
            }
        }
        Ok(())
    }

    async fn stop(&self) -> bookclerk_integrations::Result<()> {
        self.poll_epoch.fetch_add(1, Ordering::SeqCst);
        self.poll_cancel.store(true, Ordering::SeqCst);
        if self.session.has_capability("shutdown") || self.session.has_capability("stop") {
            let _ = self.int_call(|stub| async move { stub.stop().await }).await;
        }
        Ok(())
    }

    async fn on_event(&self, event: &IntegrationEvent) -> bookclerk_integrations::Result<()> {
        let _ = self.deliver_domain_event(domain_event_from(event)).await?;
        Ok(())
    }

    async fn deliver_domain_event(
        &self,
        event: DomainEvent,
    ) -> bookclerk_integrations::Result<EventResult> {
        self.deliver_domain_event_cancelable(event, Arc::new(AtomicBool::new(false)))
            .await
    }

    async fn deliver_domain_event_cancelable(
        &self,
        event: DomainEvent,
        cancel: Arc<AtomicBool>,
    ) -> bookclerk_integrations::Result<EventResult> {
        if !self.session.has_capability("onEvent") {
            return Ok(EventResult::Retry {
                retry_at_unix_ms: 0,
                reason: "onEvent capability not granted".into(),
            });
        }
        Ok(self
            .session
            .integration_cancelable(self.ctx.clone(), cancel, |stub| async move {
                stub.on_event(event).await
            })
            .await?)
    }

    fn event_subscriptions(&self) -> Vec<EventSubscription> {
        self.event_subscriptions.clone()
    }

    async fn health(&self) -> bookclerk_integrations::Result<IntegrationHealth> {
        if !self.session.has_capability("health") {
            return Ok(IntegrationHealth {
                id: self.id().to_string(),
                enabled: self.enabled,
                ok: true,
                detail: Some("external plugin (no health method)".into()),
            });
        }
        let dto = self
            .int_call(|stub| async move { stub.health().await })
            .await?;
        Ok(IntegrationHealth {
            id: self.id().to_string(),
            enabled: self.enabled,
            ok: dto.ok,
            detail: if dto.detail.is_empty() {
                None
            } else {
                Some(dto.detail)
            },
        })
    }

    fn supports_library_scan(&self) -> bool {
        self.session.has_capability("scanLibrary")
    }

    async fn scan_library(&self, force: bool) -> bookclerk_integrations::Result<()> {
        self.int_call(
            move |stub| async move { stub.scan_library(ScanLibraryParams { force }).await },
        )
        .await
    }

    fn supports_listening_sync(&self) -> bool {
        self.session.has_capability("syncListening")
    }

    async fn sync_listening_progress(
        &self,
        library: &bookclerk_library::LibraryStore,
    ) -> bookclerk_integrations::Result<usize> {
        let rows = self
            .int_call(|stub| async move { stub.sync_listening().await })
            .await?;
        let items: Vec<bookclerk_integrations::ListeningProgressSnapshot> = rows
            .into_iter()
            .map(|row| bookclerk_integrations::ListeningProgressSnapshot {
                external_user_id: row.external_user_id,
                external_item_id: row.external_item_id,
                identity_id: row.identity_id,
                title: row.title,
                authors: row.authors,
                asin: row.asin,
                isbn: row.isbn,
                progress: row.progress,
                current_time_seconds: row.current_time_seconds,
                duration_seconds: row.duration_seconds,
                is_finished: row.is_finished,
                last_listened_at: row
                    .last_listened_at_unix_ms
                    .and_then(|ms| i64::try_from(ms).ok())
                    .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis),
            })
            .collect();
        bookclerk_integrations::upsert_listening_snapshots(library, self.id(), &items).await
    }

    async fn diagnose(&self) -> bookclerk_integrations::Result<Vec<String>> {
        if !self.session.has_capability("diagnose") {
            let h = self.health().await?;
            return Ok(vec![format!(
                "{} enabled={} ok={} {}",
                h.id,
                h.enabled,
                h.ok,
                h.detail.unwrap_or_default()
            )]);
        }
        self.int_call(|stub| async move { stub.diagnose().await })
            .await
    }

    fn supports_credential_login(&self) -> bool {
        self.allow_credential_login && self.session.has_capability("authenticateUser")
    }

    async fn authenticate_user(
        &self,
        username: &str,
        password: &str,
    ) -> bookclerk_integrations::Result<ExternalUser> {
        self.session.require_binding("secrets")?;
        let params = AuthenticateUserParams {
            username: username.to_string(),
            password: password.to_string(),
        };
        let user = self
            .int_call(move |stub| async move { stub.authenticate_user(params).await })
            .await?;
        Ok(ExternalUser {
            provider: if user.provider.is_empty() {
                self.id().to_string()
            } else {
                user.provider
            },
            external_user_id: user.external_user_id,
            display_name: user.display_name,
            access_token: user.access_token,
        })
    }

    fn portal_brand(&self) -> Option<Brand> {
        self.brand
    }

    async fn provided_oidc_clients(
        &self,
    ) -> bookclerk_integrations::Result<Vec<ProvidedOidcClient>> {
        match self.session.oidc_clients().await {
            Ok(clients) => Ok(clients
                .into_iter()
                .map(|t| {
                    let display_name = t.display_name_or_id().to_string();
                    let default_scopes = t.scopes_or_default();
                    ProvidedOidcClient {
                        client_id: t.client_id,
                        display_name,
                        callback_path: t.callback_path,
                        public_client: t.public_client,
                        default_scopes,
                        issue_refresh_token: t.issue_refresh_token,
                        origin_config_key: t.origin_config_key,
                    }
                })
                .collect()),
            Err(err) => {
                tracing::debug!(
                    plugin = %self.session.id(),
                    %err,
                    "oidcClients RPC unavailable; falling back to plugin.toml"
                );
                Ok(Vec::new())
            }
        }
    }
}

/// Maps a host integration event onto a versioned [`DomainEvent`].
fn domain_event_from(event: &IntegrationEvent) -> DomainEvent {
    let (event_type, payload_val) = match event {
        IntegrationEvent::BookAcquired {
            book,
            storage_key,
            absolute_path: _,
        } => {
            let title_id = if !book.uuid.is_empty() {
                book.uuid.clone()
            } else {
                book.product_id.clone()
            };
            (
                "book_acquired",
                serde_json::json!({
                    "type": "book_acquired",
                    "payload": {
                        "titleId": title_id,
                        "source": book.source.clone(),
                        "asin": book.asin,
                        "isbn": book.isbn,
                        "pathKeys": vec![storage_key.clone()],
                    }
                }),
            )
        }
        IntegrationEvent::ExternalUserObserved {
            provider,
            external_user_id,
            display_name,
        } => (
            "config_changed",
            serde_json::json!({
                "type": "config_changed",
                "payload": {
                    "config": {
                        "externalUserObserved": {
                            "provider": provider,
                            "externalUserId": external_user_id,
                            "displayName": display_name,
                        }
                    }
                }
            }),
        ),
    };
    let occurred_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    DomainEvent {
        event_id: format!("{event_type}-{occurred_at_unix_ms}"),
        event_type: event_type.to_string(),
        schema_version: 1,
        occurred_at_unix_ms,
        deduplication_key: event_type.to_string(),
        delivery_attempt: 1,
        payload: serde_json::to_vec(&payload_val).unwrap_or_default(),
        ..DomainEvent::default()
    }
}

/// Copies a describe brand into a `'static` [`Brand`] (strings are leaked once at load).
fn brand_from_abi(brand: Option<&bookclerk_plugin_sdk::Brand>) -> Option<Brand> {
    let b = brand.filter(|b| !b.id.is_empty())?;
    Some(Brand {
        id: Box::leak(b.id.clone().into_boxed_str()),
        name: Box::leak(b.name.clone().into_boxed_str()),
        bg: Box::leak(b.bg.clone().into_boxed_str()),
        fg: Box::leak(b.fg.clone().into_boxed_str()),
        accent: Box::leak(b.accent.clone().into_boxed_str()),
        icon_url: Box::leak(b.icon_url.clone().unwrap_or_default().into_boxed_str()),
    })
}

/// Converts plugin settings TOML to JSON for guest spawn (tables, arrays, and datetimes).
fn toml_to_json(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::Number((*i).into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(a) => Value::Array(a.iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => Value::Object(
            t.iter()
                .map(|(k, v)| (k.clone(), toml_to_json(v)))
                .collect(),
        ),
    }
}
