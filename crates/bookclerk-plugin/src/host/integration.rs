//! [`Integration`] adapter over an external plugin process.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bookclerk_config::Config;
use bookclerk_integrations::{
    Brand, ExternalUser, Integration, IntegrationContext, IntegrationEvent, IntegrationHealth,
    IntegrationRegistry,
};
use serde_json::Value;
use tracing::warn;

use crate::discover::DiscoveredPlugin;
use crate::protocol::{methods, BookAcquiredDto, EventPollResultDto, HealthDto};
use crate::rpc::PluginClient;
use crate::Result;

/// External integration backed by a discovered plugin binary.
pub struct ExternalIntegration {
    client: Arc<PluginClient>,
    display_name: String,
    enabled: bool,
    brand: Option<Brand>,
    allow_credential_login: bool,
}

impl ExternalIntegration {
    /// Spawn and handshake an integration plugin.
    pub async fn spawn(plugin: &DiscoveredPlugin, config: &Config) -> Result<Self> {
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
        let client = PluginClient::spawn(
            &plugin.manifest.id,
            &plugin.command,
            &plugin.manifest.args,
            &plugin.root,
            config_json,
        )
        .await?;
        let hs = client.handshake().clone();
        let display_name = hs
            .display_name
            .clone()
            .or_else(|| plugin.manifest.name.clone())
            .unwrap_or_else(|| plugin.manifest.id.clone());
        let brand = brand_from_dto(hs.brand.as_ref());
        Ok(Self {
            client: Arc::new(client),
            display_name,
            enabled: true,
            brand,
            allow_credential_login,
        })
    }
}

/// Discover and register external integration plugins.
///
/// Duplicate `(kind, id)` claims among discovered manifests are a hard error
/// (from [`crate::discover_plugins`]). When an external id is already registered
/// in-process (dual-load `register()` path), the external copy is skipped.
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
        self.client.plugin_id()
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    async fn start(&self, ctx: IntegrationContext) -> bookclerk_integrations::Result<()> {
        if self.client.has_capability("start") {
            let _: Value = self
                .client
                .call(methods::START, Value::Object(Default::default()))
                .await?;
        }
        // Host polls `event_poll` and kicks off core workflows (e.g. claim tickets).
        // The plugin remains oblivious to what the host does with the signal.
        if self.client.has_capability("event_poll") {
            if let Some(on_user) = ctx.on_external_user {
                let client = self.client.clone();
                let plugin_id = self.id().to_string();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        match client
                            .call::<EventPollResultDto>(
                                methods::EVENT_POLL,
                                Value::Object(Default::default()),
                            )
                            .await
                        {
                            Ok(dto) => {
                                for user in dto.users {
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

    async fn on_event(&self, event: &IntegrationEvent) -> bookclerk_integrations::Result<()> {
        if !self.client.has_capability("on_event") {
            return Ok(());
        }
        let params = match event {
            IntegrationEvent::BookAcquired {
                book,
                storage_key,
                absolute_path,
            } => serde_json::json!({
                "event": "book_acquired",
                "payload": BookAcquiredDto {
                    book: serde_json::to_value(book.as_ref()).unwrap_or(Value::Null),
                    storage_key: storage_key.clone(),
                    absolute_path: absolute_path.as_ref().map(|p| p.display().to_string()),
                }
            }),
            IntegrationEvent::ExternalUserObserved {
                provider,
                external_user_id,
                display_name,
            } => serde_json::json!({
                "event": "external_user_observed",
                "payload": {
                    "provider": provider,
                    "external_user_id": external_user_id,
                    "display_name": display_name,
                }
            }),
        };
        let _: Value = self.client.call(methods::ON_EVENT, params).await?;
        Ok(())
    }

    async fn health(&self) -> bookclerk_integrations::Result<IntegrationHealth> {
        if !self.client.has_capability("health") {
            return Ok(IntegrationHealth {
                id: self.id().to_string(),
                enabled: self.enabled,
                ok: true,
                detail: Some("external plugin (no health method)".into()),
            });
        }
        let dto: HealthDto = self
            .client
            .call(methods::HEALTH, Value::Object(Default::default()))
            .await?;
        Ok(IntegrationHealth {
            id: dto.id,
            enabled: dto.enabled,
            ok: dto.ok,
            detail: dto.detail,
        })
    }

    fn supports_library_scan(&self) -> bool {
        self.client.has_capability("scan_library")
    }

    async fn scan_library(&self, force: bool) -> bookclerk_integrations::Result<()> {
        let _: Value = self
            .client
            .call(methods::SCAN_LIBRARY, serde_json::json!({ "force": force }))
            .await?;
        Ok(())
    }

    fn supports_listening_sync(&self) -> bool {
        self.client.has_capability("sync_listening")
    }

    async fn sync_listening_progress(
        &self,
        library: &bookclerk_library::LibraryStore,
    ) -> bookclerk_integrations::Result<usize> {
        let dto: crate::protocol::SyncListeningResultDto = self
            .client
            .call(methods::SYNC_LISTENING, Value::Object(Default::default()))
            .await?;
        let items: Vec<bookclerk_integrations::ListeningProgressSnapshot> = dto
            .items
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
                last_listened_at: row.last_listened_at,
            })
            .collect();
        bookclerk_integrations::upsert_listening_snapshots(library, self.id(), &items).await
    }

    async fn diagnose(&self) -> bookclerk_integrations::Result<Vec<String>> {
        if !self.client.has_capability("diagnose") {
            let h = self.health().await?;
            return Ok(vec![format!(
                "{} enabled={} ok={} {}",
                h.id,
                h.enabled,
                h.ok,
                h.detail.unwrap_or_default()
            )]);
        }
        let lines: Vec<String> = self
            .client
            .call(methods::DIAGNOSE, Value::Object(Default::default()))
            .await?;
        Ok(lines)
    }

    fn supports_credential_login(&self) -> bool {
        self.allow_credential_login && self.client.has_capability("authenticate_user")
    }

    async fn authenticate_user(
        &self,
        username: &str,
        password: &str,
    ) -> bookclerk_integrations::Result<ExternalUser> {
        let user: ExternalUser = self
            .client
            .call(
                methods::AUTHENTICATE_USER,
                serde_json::json!({ "username": username, "password": password }),
            )
            .await?;
        Ok(user)
    }

    fn portal_brand(&self) -> Option<Brand> {
        self.brand
    }
}

fn brand_from_dto(dto: Option<&crate::protocol::BrandDto>) -> Option<Brand> {
    let b = dto?;
    Some(Brand {
        id: Box::leak(b.id.clone().into_boxed_str()),
        name: Box::leak(b.name.clone().into_boxed_str()),
        bg: Box::leak(b.bg.clone().into_boxed_str()),
        fg: Box::leak(b.fg.clone().into_boxed_str()),
        accent: Box::leak(b.accent.clone().into_boxed_str()),
        icon_url: Box::leak(b.icon_url.clone().into_boxed_str()),
    })
}

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
