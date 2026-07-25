//! [`Integration`] adapter over an external plugin process.

use std::sync::Arc;

use async_trait::async_trait;
use libation_config::Config;
use libation_integrations::{
    ExternalUser, Integration, IntegrationContext, IntegrationEvent, IntegrationHealth,
    IntegrationRegistry,
};
use serde_json::Value;

use crate::discover::DiscoveredPlugin;
use crate::protocol::{methods, BookLiberatedDto, HealthDto};
use crate::rpc::PluginClient;
use crate::Result;

/// External integration backed by a discovered plugin binary.
pub struct ExternalIntegration {
    client: PluginClient,
    display_name: String,
    enabled: bool,
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
        let client = PluginClient::spawn(
            &plugin.manifest.id,
            &plugin.command,
            &plugin.manifest.args,
            &plugin.root,
            config_json,
        )
        .await?;
        let display_name = client
            .handshake()
            .display_name
            .clone()
            .or_else(|| plugin.manifest.name.clone())
            .unwrap_or_else(|| plugin.manifest.id.clone());
        Ok(Self {
            client,
            display_name,
            enabled: true,
        })
    }
}

/// Discover and register external integration plugins.
///
/// Duplicate `(kind, id)` claims among discovered manifests are a hard error
/// (from [`crate::discover_plugins`]). An external id that collides with an
/// already-registered integration is also fatal.
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
            return Err(crate::PluginError::message(format!(
                "external integration plugin id `{}` conflicts with an already registered integration ({})",
                plugin.manifest.id,
                plugin.root.join("plugin.toml").display()
            )));
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

    async fn start(&self, _ctx: IntegrationContext) -> libation_integrations::Result<()> {
        if !self.client.has_capability("start") {
            return Ok(());
        }
        let _: Value = self
            .client
            .call(methods::START, Value::Object(Default::default()))
            .await?;
        Ok(())
    }

    async fn on_event(&self, event: &IntegrationEvent) -> libation_integrations::Result<()> {
        if !self.client.has_capability("on_event") {
            return Ok(());
        }
        let params = match event {
            IntegrationEvent::BookLiberated {
                book,
                storage_key,
                absolute_path,
            } => serde_json::json!({
                "event": "book_liberated",
                "payload": BookLiberatedDto {
                    book: book.as_ref().clone(),
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

    async fn health(&self) -> libation_integrations::Result<IntegrationHealth> {
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

    async fn scan_library(&self, force: bool) -> libation_integrations::Result<()> {
        let _: Value = self
            .client
            .call(methods::SCAN_LIBRARY, serde_json::json!({ "force": force }))
            .await?;
        Ok(())
    }

    async fn diagnose(&self) -> libation_integrations::Result<Vec<String>> {
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
        self.client.has_capability("authenticate_user")
    }

    async fn authenticate_user(
        &self,
        username: &str,
        password: &str,
    ) -> libation_integrations::Result<ExternalUser> {
        let user: ExternalUser = self
            .client
            .call(
                methods::AUTHENTICATE_USER,
                serde_json::json!({ "username": username, "password": password }),
            )
            .await?;
        Ok(user)
    }
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
