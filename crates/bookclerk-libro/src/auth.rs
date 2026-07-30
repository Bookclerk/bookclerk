//! Libro.fm credential payload (stored as JSON in the `encrypted_secrets` DB table).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Credential envelope for one Libro.fm account.
///
/// Persisted as JSON in `encrypted_secrets` (see [`crate::db`]); never written
/// to disk under `Accounts/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibroAuthFile {
    pub access_token: String,
    #[serde(default = "default_token_type")]
    pub token_type: String,
    /// Absolute expiry when known (RFC3339 when serialized).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Marketplace / locale hint (not part of the mobile API; stored for Bookclerk).
    #[serde(default = "default_marketplace")]
    pub marketplace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

fn default_token_type() -> String {
    String::from("Bearer")
}

fn default_marketplace() -> String {
    String::from("us")
}

impl LibroAuthFile {
    /// Stable account id for library rows: prefer user id, else email.
    #[must_use]
    pub fn account_id(&self) -> &str {
        self.user_id.as_deref().unwrap_or(self.email.as_str())
    }
}
