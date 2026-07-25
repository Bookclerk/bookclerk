//! Integration events and shared types.

use std::path::PathBuf;

use libation_library::BookRecord;
use serde::{Deserialize, Serialize};

/// Events fan-out to registered [`crate::traits::Integration`]s.
#[derive(Debug, Clone)]
pub enum IntegrationEvent {
    /// A title was successfully liberated (or matched existing storage).
    BookLiberated {
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
