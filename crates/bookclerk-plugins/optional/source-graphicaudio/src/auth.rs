//! GraphicAudio credential payload (stored as JSON in the `encrypted_secrets` DB table).

use serde::{Deserialize, Serialize};

/// Credential envelope for one GraphicAudio account.
///
/// Persisted as JSON in `encrypted_secrets` (see [`crate::db`]); never written
/// to disk under `Accounts/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphicAudioAuthFile {
    /// Opaque device activation token from `POST activation/login`.
    /// Empty when the account was logged in with Magento-only access (`web`/`zip`).
    #[serde(default)]
    pub token: String,
    /// Device / client id sent at login (stable for this auth file; used by `device`).
    pub client_id: String,
    /// Sign-in email (also the Bookclerk account id).
    pub email: String,
    /// Marketplace / locale hint stored for Bookclerk (defaults to `us`).
    #[serde(default = "default_marketplace")]
    pub marketplace: String,
    /// Optional operator-facing account label in Accounts UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Default marketplace / locale hint (`us`) when the auth JSON omits it.
fn default_marketplace() -> String {
    String::from("us")
}

impl GraphicAudioAuthFile {
    /// Stable account id for library rows (email).
    #[must_use]
    pub fn account_id(&self) -> &str {
        self.email.as_str()
    }

    /// True when an Access App device token is present.
    #[must_use]
    pub fn has_device_token(&self) -> bool {
        !self.token.trim().is_empty()
    }
}
