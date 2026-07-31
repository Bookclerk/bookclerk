//! External Audiobookshelf integration plugin for Bookclerk.

use std::sync::Arc;

use bookclerk_plugin_integration_audiobookshelf::guest::{
    guest_authenticate_user, guest_diagnose, guest_event_poll, guest_health, guest_on_event,
    guest_scan_library, guest_start, guest_sync_listening, AbsGuestState,
};
use bookclerk_plugin_integration_audiobookshelf::BRAND;
use bookclerk_plugin_sdk::{methods, BrandDto, HandshakeResult, PluginGuest, PLUGIN_API_VERSION};
use serde_json::{json, Value};
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let state: Arc<Mutex<Option<Arc<Mutex<AbsGuestState>>>>> = Arc::new(Mutex::new(None));

    PluginGuest::serve(move |method, params| {
        let state = state.clone();
        async move {
            match method.as_str() {
                methods::HANDSHAKE => {
                    let config = params.get("config").cloned().unwrap_or(Value::Null);
                    let guest = Arc::new(Mutex::new(AbsGuestState::from_config_json(&config)));
                    *state.lock().await = Some(guest);
                    Ok(serde_json::to_value(HandshakeResult {
                        api_version: PLUGIN_API_VERSION,
                        id: "audiobookshelf".into(),
                        kind: "integration".into(),
                        display_name: Some("Audiobookshelf".into()),
                        capabilities: vec![
                            "event_poll".into(),
                            "start".into(),
                            "health".into(),
                            "diagnose".into(),
                            "scan_library".into(),
                            "sync_listening".into(),
                            "authenticate_user".into(),
                            "on_event".into(),
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
                    .unwrap())
                }
                methods::START => {
                    let guest = require_state(&state).await?;
                    guest_start(guest).await.map_err(|e| e.to_string())?;
                    Ok(json!({ "ok": true }))
                }
                methods::EVENT_POLL => {
                    let guest = require_state(&state).await?;
                    let dto = guest_event_poll(&guest).await;
                    Ok(serde_json::to_value(dto).unwrap())
                }
                methods::HEALTH => {
                    let guest = require_state(&state).await?;
                    let dto = guest_health(&guest).await.map_err(|e| e.to_string())?;
                    Ok(serde_json::to_value(dto).unwrap())
                }
                methods::DIAGNOSE => {
                    let guest = require_state(&state).await?;
                    let lines = guest_diagnose(&guest).await.map_err(|e| e.to_string())?;
                    Ok(json!(lines))
                }
                methods::SCAN_LIBRARY => {
                    let guest = require_state(&state).await?;
                    let force = params
                        .get("force")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    guest_scan_library(&guest, force)
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok(json!({ "ok": true }))
                }
                methods::SYNC_LISTENING => {
                    let guest = require_state(&state).await?;
                    let dto = guest_sync_listening(&guest)
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok(serde_json::to_value(dto).unwrap())
                }
                methods::AUTHENTICATE_USER => {
                    let guest = require_state(&state).await?;
                    let username = params
                        .get("username")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| "authenticate_user requires username".to_string())?;
                    let password = params
                        .get("password")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| "authenticate_user requires password".to_string())?;
                    let user = guest_authenticate_user(&guest, username, password)
                        .await
                        .map_err(|e| e.to_string())?;
                    // Include access_token explicitly — ExternalUser skips it on Serialize.
                    Ok(json!({
                        "provider": user.provider,
                        "external_user_id": user.external_user_id,
                        "display_name": user.display_name,
                        "access_token": user.access_token,
                    }))
                }
                methods::ON_EVENT => {
                    let guest = require_state(&state).await?;
                    guest_on_event(&guest, &params)
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok(json!({ "ok": true }))
                }
                other => Err(format!("unsupported method `{other}`")),
            }
        }
    })
    .await?;
    Ok(())
}

async fn require_state(
    state: &Mutex<Option<Arc<Mutex<AbsGuestState>>>>,
) -> Result<Arc<Mutex<AbsGuestState>>, String> {
    state
        .lock()
        .await
        .clone()
        .ok_or_else(|| "handshake required before other methods".to_string())
}
