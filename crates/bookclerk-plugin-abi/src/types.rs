//! Shared ABI DTOs used across plugin kinds (camelCase on the wire).
//!
//! Handshake, health/diagnose, plugin CLI, and stdio Workers RPC framing live
//! here. Kind-specific source / integration / output payloads are in
//! [`crate::kind`]; database connect/query types are in [`crate::db`].

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Params for [`crate::methods::handshake`].
///
/// Wire: `{ "apiVersion": 1, "config": {…} }`. `config` is the plugin's table
/// from main `config.toml` (`[sources.<id>]` / `[integrations.<id>]` / …) as
/// JSON; empty object when the table is missing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HandshakeParams {
    /// Negotiated ABI version; must equal [`crate::API_VERSION`] (wire
    /// `apiVersion`).
    pub api_version: u32,
    /// Opaque plugin config table from the host (default `{}`).
    #[serde(default)]
    pub config: Value,
}

/// Successful result of [`crate::methods::handshake`].
///
/// Required wire fields: `apiVersion`, `id`, `kind`, `capabilities`. Optional
/// fields advertise portal auth, brand colors, config option discovery, and an
/// embedded CLI schema.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HandshakeResult {
    /// ABI version the guest speaks (wire `apiVersion`); must be `1`.
    pub api_version: u32,
    /// Stable plugin id matching `plugin.toml` / install directory name.
    pub id: String,
    /// Plugin kind: `"source"`, `"integration"`, `"output"`, or `"database"`.
    pub kind: String,
    /// Human-readable name for UI lists; omitted when absent (wire
    /// `displayName`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Declared capability method names the guest implements (e.g. `health`,
    /// `login`, `fetchTitle`).
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Portal Accounts connect mode: `"oauth"` or `"password"` (wire
    /// `portalAuthMode`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portal_auth_mode: Option<String>,
    /// Optional env var name operators may set for password helpers (wire
    /// `passwordEnvVar`); never required for Accounts UI connect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_env_var: Option<String>,
    /// Alternate ids accepted for config / CLI targeting; omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Optional UI sort weight among peers of the same kind (wire `sortKey`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_key: Option<u32>,
    /// Portal brand colors and icon URL for Accounts / library chrome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brand: Option<BrandDto>,
    /// Discoverable config option groups for source UIs (wire `configOptions`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_options: Vec<ConfigOptionDto>,
    /// Optional embedded CLI schema (same shape as [`crate::methods::cli_describe`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<CliSchema>,
}

/// Portal brand crossing the RPC boundary (owned strings).
///
/// Distinct from `plugin.toml` `logo`: [`Self::icon_url`] is the live URL or
/// data URI the SPA renders after handshake.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrandDto {
    /// Brand id (often matches the plugin id).
    pub id: String,
    /// Display name shown next to the brand swatch.
    pub name: String,
    /// Background CSS color (hex or named).
    pub bg: String,
    /// Foreground CSS color for text on [`Self::bg`].
    pub fg: String,
    /// Accent CSS color for highlights / CTAs.
    pub accent: String,
    /// Icon URL or data URI for the portal (wire `iconUrl`).
    pub icon_url: String,
}

/// One discoverable config option group advertised at handshake for sources.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptionDto {
    /// Config key under the plugin's `config.toml` table.
    pub key: String,
    /// Operator-facing label for the option group.
    pub label: String,
    /// Allowed selectable values for this key.
    pub values: Vec<ConfigOptionValueDto>,
}

/// One selectable value under a [`ConfigOptionDto`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptionValueDto {
    /// Value written to config when selected.
    pub id: String,
    /// Operator-facing label for this value.
    pub label: String,
}

/// Declared plugin CLI surface (`cliDescribe` / handshake `cli` / `plugin.toml`).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CliSchema {
    /// Commands exposed as `bookclerk plugins <id> <command> …`.
    #[serde(default)]
    pub commands: Vec<CliCommandSpec>,
}

/// One plugin CLI command under [`CliSchema`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CliCommandSpec {
    /// Command verb after the plugin id (for example `ping`).
    pub name: String,
    /// Short help text for `--help`; omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    /// Argument / flag specs for this command (default empty).
    #[serde(default)]
    pub args: Vec<CliArgSpec>,
}

/// Value kind for a [`CliArgSpec`] (wire lowercase: `string` / `bool` / …).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum CliArgKind {
    /// Free-form string argument (default).
    #[default]
    String,
    /// Boolean flag (`true` / `false`).
    Bool,
    /// Integer argument.
    Int,
    /// Filesystem path argument.
    Path,
}

/// One CLI argument or flag under a [`CliCommandSpec`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CliArgSpec {
    /// Internal arg name used as the key in [`CliInvokeParams::args`].
    pub name: String,
    /// Long flag without leading dashes (e.g. `message` → `--message`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long: Option<String>,
    /// Optional short flag character (e.g. `m` → `-m`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<char>,
    /// Parsed value kind (default [`CliArgKind::String`]).
    #[serde(default)]
    pub kind: CliArgKind,
    /// When true, the host rejects invoke if the arg is missing.
    #[serde(default)]
    pub required: bool,
    /// Default string form when the operator omits the arg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Help text for this arg; omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    /// When true, the arg is positional rather than a flagged option.
    #[serde(default)]
    pub positional: bool,
}

/// Params for [`crate::methods::cli_invoke`].
///
/// Wire: `{ "command": "ping", "args": { "message": "hi" } }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CliInvokeParams {
    /// Command name matching a [`CliCommandSpec::name`].
    pub command: String,
    /// Named argument values (keys match [`CliArgSpec::name`]; default `{}`).
    #[serde(default)]
    pub args: Map<String, Value>,
}

/// Result of [`crate::methods::cli_invoke`].
///
/// Wire: `{ "exitCode": 0, "stdout": "…", "stderr": "…", "json"?: … }`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CliInvokeResult {
    /// Process-style exit code (`0` = success; wire `exitCode`).
    #[serde(default)]
    pub exit_code: i32,
    /// Captured standard output text.
    #[serde(default)]
    pub stdout: String,
    /// Captured standard error text.
    #[serde(default)]
    pub stderr: String,
    /// Optional structured payload for machine consumers; omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json: Option<Value>,
}

/// Result of [`crate::methods::health`] for branded guests (optional identity).
///
/// Prefer this shape for new guests. Host adapters that require `id` / `enabled`
/// may still deserialize [`HealthDto`].
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HealthResult {
    /// When true, the guest considers itself healthy enough for traffic.
    #[serde(default)]
    pub ok: bool,
    /// Plugin id echo; omitted when the guest does not duplicate handshake id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Whether the guest believes it is enabled in config; omitted when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Short human detail for CLI / UI status lines; omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Serializable health payload used by host adapters (required `id` / `enabled`).
///
/// Prefer [`HealthResult`] for new branded guests; this shape remains for
/// existing host deserialization and first-party guests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HealthDto {
    /// Plugin id (required for older host adapters).
    pub id: String,
    /// Whether the plugin is enabled in operator config.
    pub enabled: bool,
    /// When true, connectivity / config checks passed.
    pub ok: bool,
    /// Optional human detail string (default `None`).
    #[serde(default)]
    pub detail: Option<String>,
}

/// Result of [`crate::methods::diagnose`].
///
/// Each line is printed by `bookclerk plugins diagnose` / the control plane.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiagnoseResult {
    /// Human-readable probe lines (default empty).
    #[serde(default)]
    pub lines: Vec<String>,
}

/// Stdio Workers RPC request frame (no `jsonrpc` field).
///
/// One newline-delimited JSON object on stdin. `method` is a camelCase name
/// from [`crate::methods`]. `params` holds the method-specific DTO or is
/// omitted/`null` for no-arg methods.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    /// Correlation id echoed on [`RpcResponse`] (number or string).
    pub id: Value,
    /// Wire method name (for example `"handshake"`, `"fetchTitle"`).
    pub method: String,
    /// Method params JSON; default / omitted when the method takes none.
    #[serde(default)]
    pub params: Option<Value>,
}

/// Stdio Workers RPC response frame.
///
/// Exactly one of [`Self::result`] or [`Self::error`] should be set for a
/// completed call. `id` matches the request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    /// Correlation id matching [`RpcRequest::id`].
    pub id: Value,
    /// Successful JSON result payload; omitted on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Structured failure; omitted on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<crate::PluginError>,
}
