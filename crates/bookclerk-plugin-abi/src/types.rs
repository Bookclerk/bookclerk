//! Shared ABI DTOs (camelCase on the wire).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// `handshake` params.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HandshakeParams {
    pub api_version: u32,
    #[serde(default)]
    pub config: Value,
}

/// `handshake` result.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HandshakeResult {
    pub api_version: u32,
    pub id: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portal_auth_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_env_var: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_key: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brand: Option<BrandDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_options: Vec<ConfigOptionDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<CliSchema>,
}

/// Portal brand crossing the RPC boundary (owned strings).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrandDto {
    pub id: String,
    pub name: String,
    pub bg: String,
    pub fg: String,
    pub accent: String,
    pub icon_url: String,
}

/// Config option discovery for sources.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptionDto {
    pub key: String,
    pub label: String,
    pub values: Vec<ConfigOptionValueDto>,
}

/// One selectable value under a [`ConfigOptionDto`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptionValueDto {
    pub id: String,
    pub label: String,
}

/// Declared CLI surface.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CliSchema {
    #[serde(default)]
    pub commands: Vec<CliCommandSpec>,
}

/// One plugin CLI command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CliCommandSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    #[serde(default)]
    pub args: Vec<CliArgSpec>,
}

/// CLI argument kind.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum CliArgKind {
    #[default]
    String,
    Bool,
    Int,
    Path,
}

/// One CLI argument.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CliArgSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<char>,
    #[serde(default)]
    pub kind: CliArgKind,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    #[serde(default)]
    pub positional: bool,
}

/// `cliInvoke` params.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CliInvokeParams {
    pub command: String,
    #[serde(default)]
    pub args: Map<String, Value>,
}

/// `cliInvoke` result.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CliInvokeResult {
    #[serde(default)]
    pub exit_code: i32,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json: Option<Value>,
}

/// `health` result (branded trait / schema — optional identity fields).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HealthResult {
    #[serde(default)]
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
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
    pub id: String,
    pub enabled: bool,
    pub ok: bool,
    #[serde(default)]
    pub detail: Option<String>,
}

/// `diagnose` result.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiagnoseResult {
    #[serde(default)]
    pub lines: Vec<String>,
}

/// Stdio Workers RPC request frame (no `jsonrpc` field).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

/// Stdio Workers RPC response frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<crate::PluginError>,
}
