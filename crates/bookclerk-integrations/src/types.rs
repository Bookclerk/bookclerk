//! Integration events and shared types.

use std::path::PathBuf;

use bookclerk_library::BookRecord;
use serde::{Deserialize, Serialize};

/// Events fan-out to registered [`crate::traits::Integration`]s.
#[derive(Debug, Clone)]
pub enum IntegrationEvent {
    /// A title was successfully acquired (or matched existing storage).
    BookAcquired {
        book: Box<BookRecord>,
        storage_key: String,
        absolute_path: Option<PathBuf>,
    },
    /// An external identity was observed (e.g. ABS user created).
    ExternalUserObserved {
        provider: String,
        external_user_id: String,
        display_name: Option<String>,
    },
}

/// Health snapshot for one integration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationHealth {
    pub id: String,
    pub enabled: bool,
    pub ok: bool,
    pub detail: Option<String>,
}

/// External user resolved via integration credential login or watcher.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalUser {
    pub provider: String,
    pub external_user_id: String,
    pub display_name: Option<String>,
    /// Ephemeral token from the remote system (e.g. ABS JWT). Never persisted.
    #[serde(default, skip_serializing)]
    pub access_token: Option<String>,
}

/// One listening / progress row from any integration (ABS, plugins, …).
///
/// Hosts upsert these into the generic `listening_progress` table; ranking never
/// sees the originating adapter—only these fields.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ListeningProgressSnapshot {
    pub external_user_id: String,
    pub external_item_id: String,
    #[serde(default)]
    pub identity_id: Option<i64>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub authors: Option<String>,
    #[serde(default)]
    pub asin: Option<String>,
    #[serde(default)]
    pub isbn: Option<String>,
    #[serde(default)]
    pub progress: Option<f64>,
    #[serde(default)]
    pub current_time_seconds: Option<f64>,
    #[serde(default)]
    pub duration_seconds: Option<f64>,
    #[serde(default)]
    pub is_finished: bool,
    #[serde(default)]
    pub last_listened_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Result of syncing listening progress across one or more integrations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncListeningSummary {
    pub upserted: usize,
    pub by_provider: Vec<SyncListeningProviderResult>,
}

/// Per-integration outcome from [`crate::IntegrationRegistry::sync_listening_progress_all`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncListeningProviderResult {
    pub id: String,
    pub upserted: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
