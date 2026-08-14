//! External Audiobookshelf integration plugin for Bookclerk.

use std::sync::Arc;

use async_trait::async_trait;
use bookclerk_plugin_integration_audiobookshelf::guest::{
    guest_authenticate_user, guest_diagnose, guest_event_poll, guest_health, guest_on_event,
    guest_scan_library, guest_start, guest_sync_listening, AbsGuestState,
};
use bookclerk_plugin_integration_audiobookshelf::BRAND;
use bookclerk_plugin_sdk::{
    AuthenticateUserParams, BookclerkPlugin, BookclerkPluginGuest, BrandDto, DiagnoseResult,
    EventPollResultDto, ExternalUserDto, HandshakeParams, HandshakeResult, HealthResult,
    HostToPluginEvent, PluginError, ScanLibraryParams, SyncListeningResultDto, PLUGIN_API_VERSION,
};
use tokio::sync::Mutex;

/// Audiobookshelf integration guest; state is created at handshake.
struct AbsPlugin {
    /// Shared guest state after handshake; `None` until the host calls handshake.
    state: Mutex<Option<Arc<Mutex<AbsGuestState>>>>,
}

impl AbsPlugin {
    /// Empty plugin; handshake must run before any other RPC.
    fn new() -> Self {
        Self {
            state: Mutex::new(None),
        }
    }

    /// Returns handshake state, or `invalid_params` if handshake has not run.
    async fn require_state(&self) -> Result<Arc<Mutex<AbsGuestState>>, PluginError> {
        self.state
            .lock()
            .await
            .clone()
            .ok_or_else(|| PluginError::invalid_params("handshake required before other methods"))
    }
}

#[async_trait]
impl BookclerkPlugin for AbsPlugin {
    async fn handshake(&self, params: HandshakeParams) -> Result<HandshakeResult, PluginError> {
        let guest = Arc::new(Mutex::new(AbsGuestState::from_config_json(&params.config)));
        *self.state.lock().await = Some(guest);
        Ok(HandshakeResult {
            api_version: PLUGIN_API_VERSION,
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

    async fn start(&self) -> Result<(), PluginError> {
        let guest = self.require_state().await?;
        guest_start(guest)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))
    }

    async fn poll_events(&self) -> Result<EventPollResultDto, PluginError> {
        let guest = self.require_state().await?;
        Ok(guest_event_poll(&guest).await)
    }

    async fn health(&self) -> Result<HealthResult, PluginError> {
        let guest = self.require_state().await?;
        let dto = guest_health(&guest)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        Ok(HealthResult {
            ok: dto.ok,
            id: Some(dto.id),
            enabled: Some(dto.enabled),
            detail: dto.detail,
        })
    }

    async fn diagnose(&self) -> Result<DiagnoseResult, PluginError> {
        let guest = self.require_state().await?;
        let lines = guest_diagnose(&guest)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        Ok(DiagnoseResult { lines })
    }

    async fn scan_library(&self, params: ScanLibraryParams) -> Result<(), PluginError> {
        let guest = self.require_state().await?;
        guest_scan_library(&guest, params.force)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))
    }

    async fn sync_listening(&self) -> Result<SyncListeningResultDto, PluginError> {
        let guest = self.require_state().await?;
        guest_sync_listening(&guest)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))
    }

    async fn authenticate_user(
        &self,
        params: AuthenticateUserParams,
    ) -> Result<ExternalUserDto, PluginError> {
        let guest = self.require_state().await?;
        let user = guest_authenticate_user(&guest, &params.username, &params.password)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        Ok(ExternalUserDto {
            provider: user.provider,
            external_user_id: user.external_user_id,
            display_name: user.display_name,
            access_token: user.access_token,
        })
    }

    async fn on_event(&self, event: HostToPluginEvent) -> Result<(), PluginError> {
        let guest = self.require_state().await?;
        let params =
            serde_json::to_value(&event).map_err(|e| PluginError::internal(e.to_string()))?;
        guest_on_event(&guest, &params)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    BookclerkPluginGuest::serve(AbsPlugin::new()).await?;
    Ok(())
}
