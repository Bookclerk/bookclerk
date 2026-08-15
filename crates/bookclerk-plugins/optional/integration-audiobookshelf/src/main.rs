//! External Audiobookshelf integration plugin for Bookclerk.

#![allow(clippy::missing_docs_in_private_items)]

use std::sync::Arc;

use async_trait::async_trait;
use bookclerk_plugin_integration_audiobookshelf::guest::{
    guest_authenticate_user, guest_diagnose, guest_event_poll, guest_health, guest_on_event,
    guest_scan_library, guest_start, guest_sync_listening, AbsGuestState,
};
use bookclerk_plugin_integration_audiobookshelf::BRAND;
use bookclerk_plugin_sdk::v2::{
    decode_json, encode_json, DomainEvent, EventResult, HealthOk, Integration, IntegrationContext,
    PluginDescribe, PluginRoot, ScalarLimits, FEATURE_SCALAR_LIMITS, PRODUCT_API_VERSION,
};
use bookclerk_plugin_sdk::{
    serve, AuthenticateUserParams, BrandDto, HandshakeResult, PluginError, ScanLibraryParams,
};
use serde_json::Value;
use tokio::sync::Mutex;

/// Audiobookshelf integration guest; state is created from [`IntegrationContext`].
struct AbsRoot {
    /// Shared guest state after the first `integration()` factory call.
    state: Mutex<Option<Arc<Mutex<AbsGuestState>>>>,
}

impl AbsRoot {
    fn new() -> Self {
        Self {
            state: Mutex::new(None),
        }
    }

    async fn state_from_context(
        &self,
        context: &IntegrationContext,
    ) -> Result<Arc<Mutex<AbsGuestState>>, PluginError> {
        let mut slot = self.state.lock().await;
        if slot.is_none() {
            let config = if context.json.trim().is_empty() {
                Value::Object(Default::default())
            } else {
                serde_json::from_str(&context.json)
                    .unwrap_or_else(|_| Value::Object(Default::default()))
            };
            *slot = Some(Arc::new(Mutex::new(AbsGuestState::from_config_json(
                &config,
            ))));
        }
        Ok(slot.as_ref().expect("ABS guest state initialized").clone())
    }
}

fn describe_metadata() -> Result<String, PluginError> {
    encode_json(HandshakeResult {
        api_version: PRODUCT_API_VERSION,
        id: "audiobookshelf".into(),
        kind: "integration".into(),
        display_name: Some("Audiobookshelf".into()),
        capabilities: vec![
            "pollEvents".into(),
            "start".into(),
            "health".into(),
            "diagnose".into(),
            "scanLibrary".into(),
            "syncListening".into(),
            "authenticateUser".into(),
            "onEvent".into(),
        ],
        aliases: vec!["abs".into()],
        brand: Some(BrandDto {
            id: BRAND.id.into(),
            name: BRAND.name.into(),
            bg: BRAND.bg.into(),
            fg: BRAND.fg.into(),
            accent: BRAND.accent.into(),
            icon_url: BRAND.icon_url.into(),
        }),
        ..HandshakeResult::default()
    })
}

#[async_trait(?Send)]
impl PluginRoot for AbsRoot {
    async fn describe(&self) -> Result<PluginDescribe, PluginError> {
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: "audiobookshelf".into(),
            kind: "integration".into(),
            display_name: Some("Audiobookshelf".into()),
            rpc_features: vec![FEATURE_SCALAR_LIMITS.into()],
            scalar_limits: ScalarLimits::default().into(),
            supported_roles: vec!["integration".into()],
            metadata_json: describe_metadata()?,
            ..PluginDescribe::default()
        })
    }

    async fn integration(
        &self,
        context: IntegrationContext,
    ) -> Result<Box<dyn Integration>, PluginError> {
        Ok(Box::new(AbsIntegration {
            state: self.state_from_context(&context).await?,
        }))
    }
}

struct AbsIntegration {
    state: Arc<Mutex<AbsGuestState>>,
}

#[async_trait(?Send)]
impl Integration for AbsIntegration {
    async fn health(&self) -> Result<HealthOk, PluginError> {
        let dto = guest_health(&self.state)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        Ok(HealthOk {
            ok: dto.ok,
            detail: dto.detail.unwrap_or_default(),
        })
    }

    async fn diagnose(&self) -> Result<String, PluginError> {
        let lines = guest_diagnose(&self.state)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        encode_json(lines)
    }

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

    async fn start(&self) -> Result<(), PluginError> {
        guest_start(Arc::clone(&self.state))
            .await
            .map_err(|e| PluginError::internal(e.to_string()))
    }

    async fn scan_library(&self, params_json: &str) -> Result<(), PluginError> {
        let params: ScanLibraryParams = decode_json(params_json)?;
        guest_scan_library(&self.state, params.force)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))
    }

    async fn sync_listening(&self) -> Result<String, PluginError> {
        encode_json(
            guest_sync_listening(&self.state)
                .await
                .map_err(|e| PluginError::internal(e.to_string()))?,
        )
    }

    async fn authenticate_user(&self, params_json: &str) -> Result<String, PluginError> {
        let params: AuthenticateUserParams = decode_json(params_json)?;
        let user = guest_authenticate_user(&self.state, &params.username, &params.password)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        encode_json(bookclerk_plugin_sdk::ExternalUserDto {
            provider: user.provider,
            external_user_id: user.external_user_id,
            display_name: user.display_name,
            access_token: user.access_token,
        })
    }

    async fn poll_events(&self) -> Result<String, PluginError> {
        encode_json(guest_event_poll(&self.state).await)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(AbsRoot::new()).await?;
    Ok(())
}
