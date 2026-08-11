//! Typed host↔plugin event envelopes.
//!
//! - **Host → plugin** — delivered via [`crate::methods::on_event`] as
//!   [`HostToPluginEvent`] (`type` + `payload` tagged enum; discriminant
//!   `snake_case` on the wire).
//! - **Plugin → host** — workerd guests POST [`PluginToHostEvent`] through
//!   `env.HOST.notify`; native guests use the stdio reverse channel. Payload
//!   object fields use camelCase.
//!
//! See product docs on the reverse channel in `docs/plugins.md`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Host → plugin event delivered as params to [`crate::methods::on_event`].
///
/// Wire shape: `{ "type": "<snake_case>", "payload": {…} }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum HostToPluginEvent {
    /// A title was acquired and written to enabled destinations
    /// (`type: "book_acquired"`).
    #[serde(rename = "book_acquired")]
    BookAcquired(BookAcquiredPayload),
    /// A source library scan finished upserting titles
    /// (`type: "library_scan_completed"`).
    #[serde(rename = "library_scan_completed")]
    LibraryScanCompleted(LibraryScanCompletedPayload),
    /// The plugin's config table in main `config.toml` changed
    /// (`type: "config_changed"`).
    #[serde(rename = "config_changed")]
    ConfigChanged(ConfigChangedPayload),
}

/// Payload for [`HostToPluginEvent::BookAcquired`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BookAcquiredPayload {
    /// Host library title id after upsert (wire `titleId`).
    pub title_id: String,
    /// Source plugin id that owned the acquire (forced by the host).
    pub source: String,
    /// Amazon ASIN when known; omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asin: Option<String>,
    /// ISBN when known; omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isbn: Option<String>,
    /// Destination object keys written during acquire (wire `pathKeys`).
    pub path_keys: Vec<String>,
}

/// Payload for [`HostToPluginEvent::LibraryScanCompleted`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LibraryScanCompletedPayload {
    /// Source plugin id whose scan completed.
    pub source: String,
    /// Number of titles the host upserted from that scan.
    pub upserted: u64,
}

/// Payload for [`HostToPluginEvent::ConfigChanged`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigChangedPayload {
    /// Fresh plugin config table as JSON (same shape as handshake `config`).
    pub config: Value,
}

/// Plugin → host event (`env.HOST.notify` / stdio reverse channel).
///
/// Wire shape: `{ "type": "<snake_case>", "payload": {…} }`. The host buffers
/// these for the session and may kick core workflows (claim tickets, progress
/// upserts) without exposing portal details to the guest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum PluginToHostEvent {
    /// Newly observed external users from an integration
    /// (`type: "external_users"`).
    #[serde(rename = "external_users")]
    ExternalUsers(ExternalUsersPayload),
    /// Listening-progress snapshots to upsert
    /// (`type: "listening_progress"`).
    #[serde(rename = "listening_progress")]
    ListeningProgress(ListeningProgressPayload),
    /// Structured log line for the host diagnostics ring
    /// (`type: "plugin_log"`).
    #[serde(rename = "plugin_log")]
    PluginLog(PluginLogPayload),
}

/// Payload for [`PluginToHostEvent::ExternalUsers`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExternalUsersPayload {
    /// Opaque per-user JSON objects (often shaped like
    /// [`crate::kind::ExternalUserDto`]); host interprets by provider.
    pub users: Vec<Value>,
}

/// Payload for [`PluginToHostEvent::ListeningProgress`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ListeningProgressPayload {
    /// Opaque progress item JSON (often shaped like
    /// [`crate::kind::ListeningProgressDto`]).
    pub items: Vec<Value>,
}

/// Payload for [`PluginToHostEvent::PluginLog`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginLogPayload {
    /// Severity for the host log ring / stderr mirror.
    pub level: PluginLogLevel,
    /// Single-line message; secrets must already be redacted by the guest.
    pub message: String,
}

/// Severity for [`PluginLogPayload`] (wire lowercase: `debug` / `info` / …).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PluginLogLevel {
    /// Debug-level guest diagnostic.
    Debug,
    /// Informational guest message.
    Info,
    /// Warning that does not fail the RPC.
    Warn,
    /// Error-level guest diagnostic (RPC may still succeed).
    Error,
}
