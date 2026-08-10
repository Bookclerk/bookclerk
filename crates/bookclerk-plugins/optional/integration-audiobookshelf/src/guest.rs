//! Guest-process helpers for the external Audiobookshelf integration plugin.
//!
//! Opaque config JSON (from `[integrations.audiobookshelf]` / handshake) drives
//! the HTTP client. User-watch queues newly observed users for
//! [`guest_event_poll`] — the host owns claim-ticket / portal workflows.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use bookclerk_config::AudiobookshelfConfig;
use bookclerk_integrations::{ExternalUser, IntegrationError, Result};
use bookclerk_plugin_sdk::{
    EventPollResultDto, ExternalUserDto, HealthDto, ListeningProgressDto, SyncListeningResultDto,
};
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::client::AbsApiClient;
use crate::listening::collect_listening_snapshots;

const PROVIDER: &str = "audiobookshelf";

/// Shared guest state for the ABS external plugin process.
pub struct AbsGuestState {
    config: AudiobookshelfConfig,
    client: Option<AbsApiClient>,
    config_error: Option<String>,
    known_users: HashSet<String>,
    queued_users: VecDeque<ExternalUserDto>,
    watch_started: bool,
}

impl AbsGuestState {
    /// Build guest state from opaque handshake / settings JSON.
    #[must_use]
    pub fn from_config_json(config: &Value) -> Self {
        let parsed: AudiobookshelfConfig =
            serde_json::from_value(config.clone()).unwrap_or_default();
        Self::from_config(parsed)
    }

    /// Build guest state from a typed ABS config.
    #[must_use]
    pub fn from_config(config: AudiobookshelfConfig) -> Self {
        let api_key = config.api_key.clone().filter(|s| !s.is_empty());
        let (client, config_error) = match api_key {
            None => (
                None,
                Some(
                    "integrations.audiobookshelf.api_key (or BOOKCLERK_ABS_API_KEY) is required"
                        .to_string(),
                ),
            ),
            Some(api_key) => match AbsApiClient::new(config.base_url.clone(), api_key) {
                Ok(client) => (Some(client), None),
                Err(err) => (None, Some(err.to_string())),
            },
        };
        Self {
            config,
            client,
            config_error,
            known_users: HashSet::new(),
            queued_users: VecDeque::new(),
            watch_started: false,
        }
    }

    fn require_client(&self) -> Result<&AbsApiClient> {
        self.client.as_ref().ok_or_else(|| {
            IntegrationError::message(
                self.config_error
                    .clone()
                    .unwrap_or_else(|| "audiobookshelf integration is misconfigured".into()),
            )
        })
    }
}

/// Start ABS guest background work (user watch → queue for [`guest_event_poll`]).
pub async fn guest_start(state: Arc<Mutex<AbsGuestState>>) -> Result<()> {
    let (client, watch_users) = {
        let mut g = state.lock().await;
        if g.watch_started {
            return Ok(());
        }
        let client = match g.require_client() {
            Ok(c) => c.clone(),
            Err(err) => return Err(err),
        };
        let watch = g.config.watch_users;
        g.watch_started = true;
        (client, watch)
    };
    if !watch_users {
        debug!("ABS guest user watch disabled");
        return Ok(());
    }

    // Seed known set, then poll for newly observed users.
    {
        let mut g = state.lock().await;
        match client.list_users().await {
            Ok(users) => {
                for u in users {
                    g.known_users.insert(u.id);
                }
                info!(
                    count = g.known_users.len(),
                    "seeded ABS guest user watch set"
                );
            }
            Err(err) => warn!(%err, "failed to seed ABS guest users"),
        }
    }

    let watch_state = state.clone();
    let watch_client = client.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            match watch_client.list_users().await {
                Ok(users) => {
                    let mut g = watch_state.lock().await;
                    for user in users {
                        if g.known_users.insert(user.id.clone()) {
                            info!(
                                user_id = %user.id,
                                username = %user.username,
                                "ABS guest user observed"
                            );
                            g.queued_users.push_back(ExternalUserDto {
                                provider: PROVIDER.into(),
                                external_user_id: user.id,
                                display_name: Some(user.username),
                                access_token: None,
                            });
                        }
                    }
                }
                Err(err) => warn!(%err, "ABS guest user poll failed"),
            }
        }
    });
    Ok(())
}

/// Drain newly observed users queued by the watch loop.
pub async fn guest_event_poll(state: &Mutex<AbsGuestState>) -> EventPollResultDto {
    let mut g = state.lock().await;
    let users: Vec<_> = g.queued_users.drain(..).collect();
    EventPollResultDto { users }
}

/// Health check for the guest process.
pub async fn guest_health(state: &Mutex<AbsGuestState>) -> Result<HealthDto> {
    let g = state.lock().await;
    let Some(client) = g.client.as_ref() else {
        return Ok(HealthDto {
            id: PROVIDER.into(),
            enabled: g.config.enabled,
            ok: false,
            detail: g
                .config_error
                .clone()
                .or_else(|| Some("audiobookshelf integration is misconfigured".into())),
        });
    };
    match client.authorize().await {
        Ok(auth) => Ok(HealthDto {
            id: PROVIDER.into(),
            enabled: g.config.enabled,
            ok: true,
            detail: auth.user.map(|u| format!("authorized as {}", u.username)),
        }),
        Err(err) => Ok(HealthDto {
            id: PROVIDER.into(),
            enabled: g.config.enabled,
            ok: false,
            detail: Some(err.to_string()),
        }),
    }
}

/// Diagnose ABS connectivity and list libraries.
pub async fn guest_diagnose(state: &Mutex<AbsGuestState>) -> Result<Vec<String>> {
    let g = state.lock().await;
    let client = g.require_client()?;
    let auth = client.authorize().await?;
    let mut lines = Vec::new();
    if let Some(user) = auth.user {
        lines.push(format!("authorized as {} ({})", user.username, user.id));
    } else {
        lines.push("authorized (no user in response)".into());
    }
    for lib in client.list_libraries().await? {
        lines.push(format!("library {} — {}", lib.id, lib.name));
    }
    Ok(lines)
}

/// Trigger an ABS library scan.
pub async fn guest_scan_library(state: &Mutex<AbsGuestState>, force: bool) -> Result<()> {
    let g = state.lock().await;
    let client = g.require_client()?;
    let Some(library_id) = g.config.library_id.as_deref().filter(|s| !s.is_empty()) else {
        return Err(IntegrationError::message(
            "integrations.audiobookshelf.library_id unset",
        ));
    };
    client.scan_library(library_id, force).await
}

/// Collect listening progress as protocol DTOs (host upserts).
pub async fn guest_sync_listening(state: &Mutex<AbsGuestState>) -> Result<SyncListeningResultDto> {
    let g = state.lock().await;
    let client = g.require_client()?;
    let snapshots = collect_listening_snapshots(client).await?;
    Ok(SyncListeningResultDto {
        items: snapshots
            .into_iter()
            .map(|row| ListeningProgressDto {
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
            .collect(),
    })
}

/// Username/password login against ABS (when allowed).
pub async fn guest_authenticate_user(
    state: &Mutex<AbsGuestState>,
    username: &str,
    password: &str,
) -> Result<ExternalUser> {
    let g = state.lock().await;
    if !g.config.allow_credential_login {
        return Err(IntegrationError::message(
            "integrations.audiobookshelf.allow_credential_login is false",
        ));
    }
    g.require_client()?
        .authenticate_user(username, password)
        .await
}

/// Handle host-forwarded integration events (e.g. book_acquired → scan).
pub async fn guest_on_event(state: &Mutex<AbsGuestState>, params: &Value) -> Result<()> {
    let event = params
        .get("type")
        .or_else(|| params.get("event"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if event != "book_acquired" {
        return Ok(());
    }
    let (notify, library_id, client) = {
        let g = state.lock().await;
        let notify = g.config.notify_scan_on_acquire;
        let library_id = g.config.library_id.clone();
        let client = g.require_client()?.clone();
        (notify, library_id, client)
    };
    if !notify {
        return Ok(());
    }
    let Some(library_id) = library_id.filter(|s| !s.is_empty()) else {
        warn!("ABS guest scan skipped: integrations.audiobookshelf.library_id unset");
        return Ok(());
    };
    info!(%library_id, "ABS guest triggering library scan after book_acquired");
    client.scan_library(&library_id, false).await
}
