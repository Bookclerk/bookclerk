//! [`Integration`] adapter over an external plugin process.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bookclerk_config::Config;
use bookclerk_integrations::{
    Brand, ExternalUser, Integration, IntegrationContext, IntegrationEvent, IntegrationHealth,
    IntegrationRegistry,
};
use bookclerk_plugin_sdk::v2::{DomainEvent, HealthOk, PRODUCT_API_VERSION};
use serde_json::Value;
use tracing::warn;

use crate::discover::DiscoveredPlugin;
use crate::protocol::EventPollResultDto;
use crate::rpc_v2::{V2PluginSession, HOST_SHARED_ACCOUNT};
use crate::Result;

/// External integration backed by a discovered plugin binary.
pub struct ExternalIntegration {
    /// Cap'n Proto v2 session (never given `library.db`).
    session: Arc<V2PluginSession>,
    /// JSON factory context (plugin config table).
    ctx_json: String,
    /// Operator-facing name from the handshake (falls back to the manifest id).
    display_name: String,
    /// Whether this integration is enabled in host config after handshake.
    enabled: bool,
    /// Portal brand colors/icon leaked from the handshake DTO, if the guest supplied one.
    brand: Option<Brand>,
    /// When true, the host may call the guest credential-login RPC (username/password).
    allow_credential_login: bool,
    /// Cancels the host-side `event_poll` loop from [`Self::start`].
    poll_cancel: Arc<AtomicBool>,
    /// Bumped on each [`Self::start`]/[`Self::stop`] so a superseded poll loop exits.
    poll_epoch: Arc<AtomicU64>,
}

impl ExternalIntegration {
    /// Spawn and handshake an integration plugin.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn spawn(plugin: &DiscoveredPlugin, config: &Config) -> Result<Self> {
        if plugin.manifest.api_version != PRODUCT_API_VERSION {
            return Err(crate::PluginError::message(format!(
                "plugin `{}` api_version {} is not v2",
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
            V2PluginSession::spawn_for_account(
                plugin,
                config,
                config_json.clone(),
                HOST_SHARED_ACCOUNT,
            )
            .await?,
        );
        let source_config = crate::handshake_config_for_grant(session.grant(), config_json);
        let hs = session.handshake_metadata();
        let display_name = hs
            .display_name
            .clone()
            .or_else(|| plugin.manifest.name.clone())
            .unwrap_or_else(|| plugin.manifest.id.clone());
        let brand = brand_from_dto(hs.brand.as_ref());
        Ok(Self {
            session,
            ctx_json: source_config.to_string(),
            display_name,
            enabled: true,
            brand,
            allow_credential_login,
            poll_cancel: Arc::new(AtomicBool::new(false)),
            poll_epoch: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Forwards one integration RPC through the v2 session.
    ///
    /// # Errors
    ///
    /// Returns when the session call fails or params cannot be serialized.
    async fn int_call(&self, op: &str, params: Value) -> bookclerk_integrations::Result<String> {
        let raw = self
            .session
            .integration_json(
                self.ctx_json.clone(),
                op,
                serde_json::to_string(&params).unwrap_or_else(|_| "{}".into()),
            )
            .await?;
        Ok(raw)
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
                .int_call("start", Value::Object(Default::default()))
                .await;
        }
        // Host polls `event_poll` and kicks off core workflows (e.g. claim tickets).
        // The plugin remains oblivious to what the host does with the signal.
        if self.session.has_capability("pollEvents") {
            if let Some(on_user) = ctx.on_external_user {
                let epoch = self.poll_epoch.fetch_add(1, Ordering::SeqCst) + 1;
                self.poll_cancel.store(false, Ordering::SeqCst);
                let session = self.session.clone();
                let ctx_json = self.ctx_json.clone();
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
                            .integration_json(ctx_json.clone(), "pollEvents", "{}")
                            .await
                        {
                            Ok(raw) => {
                                let dto: EventPollResultDto =
                                    serde_json::from_str(&raw).unwrap_or_default();
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

    async fn stop(&self) -> bookclerk_integrations::Result<()> {
        self.poll_epoch.fetch_add(1, Ordering::SeqCst);
        self.poll_cancel.store(true, Ordering::SeqCst);
        if self.session.has_capability("shutdown") || self.session.has_capability("stop") {
            let _ = self
                .int_call("stop", Value::Object(Default::default()))
                .await;
        }
        Ok(())
    }

    async fn on_event(&self, event: &IntegrationEvent) -> bookclerk_integrations::Result<()> {
        if !self.session.has_capability("onEvent") {
            return Ok(());
        }
        let domain = domain_event_from(event);
        let params = serde_json::to_value(&domain).unwrap_or(Value::Object(Default::default()));
        let _ = self.int_call("onEvent", params).await?;
        Ok(())
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
        let raw = self
            .int_call("health", Value::Object(Default::default()))
            .await?;
        let dto: HealthOk = serde_json::from_str(&raw).unwrap_or_default();
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
        let _ = self
            .int_call("scanLibrary", serde_json::json!({ "force": force }))
            .await?;
        Ok(())
    }

    fn supports_listening_sync(&self) -> bool {
        self.session.has_capability("syncListening")
    }

    async fn sync_listening_progress(
        &self,
        library: &bookclerk_library::LibraryStore,
    ) -> bookclerk_integrations::Result<usize> {
        let raw = self
            .int_call("syncListening", Value::Object(Default::default()))
            .await?;
        let dto: crate::protocol::SyncListeningResultDto =
            serde_json::from_str(&raw).map_err(crate::PluginError::from)?;
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
        let raw = self
            .int_call("diagnose", Value::Object(Default::default()))
            .await?;
        Ok(parse_diagnose_lines(&raw))
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
        let raw = self
            .int_call(
                "authenticateUser",
                serde_json::json!({ "username": username, "password": password }),
            )
            .await?;
        Ok(serde_json::from_str(&raw).map_err(crate::PluginError::from)?)
    }

    fn portal_brand(&self) -> Option<Brand> {
        self.brand
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
        account_id: String::new(),
        correlation_id: String::new(),
        causation_id: String::new(),
        deduplication_key: event_type.to_string(),
        delivery_attempt: 1,
        payload: serde_json::to_vec(&payload_val).unwrap_or_default(),
    }
}

/// Parses diagnose JSON (`string[]` or `{lines:[…]}`) into operator lines.
fn parse_diagnose_lines(raw: &str) -> Vec<String> {
    if let Ok(lines) = serde_json::from_str::<Vec<String>>(raw) {
        return lines;
    }
    if let Ok(obj) = serde_json::from_str::<Value>(raw) {
        if let Some(arr) = obj.get("lines").and_then(|v| v.as_array()) {
            return arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
        }
    }
    vec![raw.to_string()]
}

/// Copies a handshake brand DTO into a `'static` [`Brand`] (strings are leaked once at load).
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
