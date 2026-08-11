//! Integration events and shared types.

use std::path::PathBuf;

use bookclerk_library::BookRecord;
use serde::{Deserialize, Serialize};

/// Events fan-out to registered [`crate::traits::Integration`]s.
#[derive(Debug, Clone)]
pub enum IntegrationEvent {
    /// A title was successfully acquired (or matched existing storage).
    BookAcquired {
        /// Library book row that was acquired.
        book: Box<BookRecord>,
        /// Destination storage key written by acquire (relative or object key).
        storage_key: String,
        /// Local absolute path when the destination is on-disk; `None` for remote-only.
        absolute_path: Option<PathBuf>,
    },
    /// An external identity was observed (e.g. ABS user created).
    ExternalUserObserved {
        /// Integration / storefront id that owns this external identity.
        provider: String,
        /// Remote system's user id (never a Bookclerk user id).
        external_user_id: String,
        /// Optional human-facing name from the remote system.
        display_name: Option<String>,
    },
}

/// Health snapshot for one integration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationHealth {
    /// Integration id this health row describes (`audiobookshelf`, …).
    pub id: String,
    /// When true, this feature or account is active.
    pub enabled: bool,
    /// When true, the last health probe succeeded.
    pub ok: bool,
    /// Optional human-readable health or error detail.
    pub detail: Option<String>,
}

/// External user resolved via integration credential login or watcher.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalUser {
    /// Integration / storefront id that owns this external identity.
    pub provider: String,
    /// Remote system's user id (never a Bookclerk user id).
    pub external_user_id: String,
    /// Optional human-facing name from the remote system.
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
    /// Remote system's user id (never a Bookclerk user id).
    pub external_user_id: String,
    /// Remote library item id for progress matching.
    pub external_item_id: String,
    /// Bookclerk portal identity row id when already linked.
    #[serde(default)]
    pub identity_id: Option<i64>,
    /// Display title as shown on the storefront or library card.
    #[serde(default)]
    pub title: Option<String>,
    /// Comma-separated author names when the storefront provides them.
    #[serde(default)]
    pub authors: Option<String>,
    /// Audible / Amazon ASIN when this edition is sold on Audible.
    #[serde(default)]
    pub asin: Option<String>,
    /// Canonical ISBN-13 (or ISBN-10 normalized) when published.
    #[serde(default)]
    pub isbn: Option<String>,
    /// Fraction complete in `[0.0, 1.0]` when the remote reports it.
    #[serde(default)]
    pub progress: Option<f64>,
    /// Playback position in seconds from the start of the title.
    #[serde(default)]
    pub current_time_seconds: Option<f64>,
    /// Total title duration in seconds when known.
    #[serde(default)]
    pub duration_seconds: Option<f64>,
    /// When true, the remote marks this title as finished.
    #[serde(default)]
    pub is_finished: bool,
    /// UTC timestamp of the last playback event when known.
    #[serde(default)]
    pub last_listened_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Result of syncing listening progress across one or more integrations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncListeningSummary {
    /// Number of listening_progress rows written or updated.
    pub upserted: usize,
    /// Per-integration breakdown of upsert counts and errors.
    pub by_provider: Vec<SyncListeningProviderResult>,
}

/// Per-integration outcome from [`crate::IntegrationRegistry::sync_listening_progress_all`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncListeningProviderResult {
    /// Stable identifier for this item.
    pub id: String,
    /// Number of listening_progress rows written or updated.
    pub upserted: usize,
    /// Operator-facing error text when this step failed; `None` on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
