//! Chirp credential payload (stored as JSON in the `encrypted_secrets` DB table).

use serde::{Deserialize, Serialize};

/// Credential envelope for one Chirp account.
///
/// Persisted as JSON in `encrypted_secrets` (see [`crate::db`]); never written
/// to disk under `Accounts/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChirpAuthFile {
    /// JWT from GraphQL `signIn` (`user.token`).
    pub access_token: String,
    /// Longer-lived web JWT when present (`user.webToken`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_token: Option<String>,
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default = "default_marketplace")]
    pub marketplace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

fn default_marketplace() -> String {
    String::from("us")
}

impl ChirpAuthFile {
    /// Stable account id: prefer Chirp user id, else email.
    #[must_use]
    pub fn account_id(&self) -> &str {
        self.user_id.as_deref().unwrap_or(self.email.as_str())
    }
}
