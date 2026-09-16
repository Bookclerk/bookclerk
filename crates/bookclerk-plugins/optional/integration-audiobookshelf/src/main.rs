//! External Audiobookshelf integration plugin for Bookclerk.

#![allow(clippy::missing_docs_in_private_items)]

use std::sync::Arc;

use async_trait::async_trait;
use bookclerk_plugin_integration_audiobookshelf::guest::{
    guest_authenticate_user, guest_diagnose, guest_event_poll, guest_health, guest_on_event,
    guest_scan_library, guest_start, guest_sync_listening, AbsGuestState,
};
use bookclerk_plugin_integration_audiobookshelf::BRAND;
use bookclerk_plugin_sdk::manifest_capabilities;
use bookclerk_plugin_sdk::{
    serve, AuthenticateUserParams, Bindings, Brand, DomainEvent, Entrypoints, EventConsumer,
    EventResult, ExternalUser, HealthOk, Invocation, ListeningProgress, Oidc, OidcClientTemplate,
    PluginDescribe, PluginError, PluginWorker, RemoteLibrary, ScalarLimits, ScanLibraryParams,
    FEATURE_SCALAR_LIMITS, PRODUCT_API_VERSION,
};
use serde_json::Value;
use tokio::sync::Mutex;

/// Audiobookshelf integration guest; state is created from the `open`
/// [`Bindings`] (`CONFIG`).
struct AbsRoot {
    /// Shared guest state after the first `open()` call.
    state: Mutex<Option<Arc<Mutex<AbsGuestState>>>>,
}

impl AbsRoot {
    fn new() -> Self {
        Self {
            state: Mutex::new(None),
        }
    }

    async fn state_from_bindings(
        &self,
        bindings: &Bindings,
    ) -> Result<Arc<Mutex<AbsGuestState>>, PluginError> {
        let mut slot = self.state.lock().await;
        if slot.is_none() {
            let config = bindings
                .config
                .json_value()
                .unwrap_or_else(|_| Value::Object(Default::default()));
            *slot = Some(Arc::new(Mutex::new(AbsGuestState::from_config_json(
                &config,
            ))));
        }
        Ok(slot.as_ref().expect("ABS guest state initialized").clone())
    }
}

#[async_trait(?Send)]
impl PluginWorker for AbsRoot {
    async fn describe(&self) -> Result<PluginDescribe, PluginError> {
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: "audiobookshelf".into(),
            display_name: Some("Audiobookshelf".into()),
            rpc_features: vec![FEATURE_SCALAR_LIMITS.into()],
            scalar_limits: ScalarLimits::default().into(),
            capabilities: manifest_capabilities(include_str!("../plugin.toml"))?,
            aliases: vec!["abs".into()],
            brand: Some(Brand {
                id: BRAND.id.into(),
                name: BRAND.name.into(),
                bg: BRAND.bg.into(),
                fg: BRAND.fg.into(),
                accent: BRAND.accent.into(),
                icon_url: Some(BRAND.icon_url.into()),
            }),
            ..PluginDescribe::default()
        })
    }

    async fn open(
        &self,
        _invocation: Invocation,
        bindings: Bindings,
    ) -> Result<Entrypoints, PluginError> {
        let state = self.state_from_bindings(&bindings).await?;
        Ok(Entrypoints {
            event_consumer: Some(Box::new(AbsIntegration {
                state: Arc::clone(&state),
            })),
            remote_library: Some(Box::new(AbsIntegration {
                state: Arc::clone(&state),
            })),
            oidc: Some(Box::new(AbsIntegration { state })),
            ..Entrypoints::default()
        })
    }
}

struct AbsIntegration {
    state: Arc<Mutex<AbsGuestState>>,
}

#[async_trait(?Send)]
impl Oidc for AbsIntegration {
    async fn clients(&self) -> Result<Vec<OidcClientTemplate>, PluginError> {
        Ok(bookclerk_plugin_integration_audiobookshelf::oidc_client_templates())
    }

    async fn authenticate_user(
        &self,
        params: AuthenticateUserParams,
    ) -> Result<ExternalUser, PluginError> {
        let user = guest_authenticate_user(&self.state, &params.username, &params.password)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        Ok(ExternalUser {
            provider: user.provider,
            external_user_id: user.external_user_id,
            display_name: user.display_name,
            access_token: user.access_token,
        })
    }
}

#[async_trait(?Send)]
impl EventConsumer for AbsIntegration {
    async fn event(&self, batch: Vec<DomainEvent>) -> Result<Vec<EventResult>, PluginError> {
        let mut results = Vec::with_capacity(batch.len());
        for event in batch {
            results.push(self.on_event(event).await?);
        }
        Ok(results)
    }
}

impl AbsIntegration {
    async fn on_event(&self, event: DomainEvent) -> Result<EventResult, PluginError> {
        let params = if event.payload.is_empty() {
            serde_json::json!({ "type": event.event_type })
        } else {
            serde_json::from_slice(&event.payload)
                .unwrap_or_else(|_| serde_json::json!({ "type": event.event_type }))
        };
        guest_on_event(&self.state, &params)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        Ok(EventResult::Ack)
    }
}

#[async_trait(?Send)]
impl RemoteLibrary for AbsIntegration {
    async fn health(&self) -> Result<HealthOk, PluginError> {
        guest_health(&self.state)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))
    }

    async fn diagnose(&self) -> Result<Vec<String>, PluginError> {
        guest_diagnose(&self.state)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))
    }

    async fn start(&self) -> Result<(), PluginError> {
        guest_start(Arc::clone(&self.state))
            .await
            .map_err(|e| PluginError::internal(e.to_string()))
    }

    async fn scan_library(&self, params: ScanLibraryParams) -> Result<(), PluginError> {
        guest_scan_library(&self.state, params.force)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))
    }

    async fn sync_listening(&self) -> Result<Vec<ListeningProgress>, PluginError> {
        guest_sync_listening(&self.state)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))
    }

    async fn poll_events(&self) -> Result<Vec<ExternalUser>, PluginError> {
        Ok(guest_event_poll(&self.state).await.users)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(AbsRoot::new()).await?;
    Ok(())
}
