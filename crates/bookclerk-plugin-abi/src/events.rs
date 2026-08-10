//! Typed host↔plugin event envelopes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Host → plugin (`onEvent`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum HostToPluginEvent {
    #[serde(rename = "book_acquired")]
    BookAcquired(BookAcquiredPayload),
    #[serde(rename = "library_scan_completed")]
    LibraryScanCompleted(LibraryScanCompletedPayload),
    #[serde(rename = "config_changed")]
    ConfigChanged(ConfigChangedPayload),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BookAcquiredPayload {
    pub title_id: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isbn: Option<String>,
    pub path_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LibraryScanCompletedPayload {
    pub source: String,
    pub upserted: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigChangedPayload {
    pub config: Value,
}

/// Plugin → host (`env.HOST.notify`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum PluginToHostEvent {
    #[serde(rename = "external_users")]
    ExternalUsers(ExternalUsersPayload),
    #[serde(rename = "listening_progress")]
    ListeningProgress(ListeningProgressPayload),
    #[serde(rename = "plugin_log")]
    PluginLog(PluginLogPayload),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExternalUsersPayload {
    pub users: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ListeningProgressPayload {
    pub items: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginLogPayload {
    pub level: PluginLogLevel,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PluginLogLevel {
    Debug,
    Info,
    Warn,
    Error,
}
