//! Output-plugin configuration DTOs carried as `application/json` inside
//! [`crate::ExtensibleConfig`] (`DestinationContext.config`).
//!
//! Field names serialize as **camelCase** on the wire. Every method payload
//! is a typed Cap'n Proto struct generated into [`crate::generated`]; only the
//! operator-configured destination knobs remain JSON because they are
//! plugin-private configuration, not ABI method parameters.

use serde::{Deserialize, Serialize};

/// Static AWS-style credentials injected by the host (never read from guest env).
///
/// [`Debug`] redacts secret fields. Present on [`OutputS3ContextDto`] when the
/// host has resolved keys from env or `encrypted_secrets`.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct S3CredentialsDto {
    /// Access key id (wire `accessKeyId`).
    pub access_key_id: String,
    /// Secret access key (wire `secretAccessKey`).
    pub secret_access_key: String,
    /// Optional session token for temporary credentials (wire `sessionToken`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
}

impl std::fmt::Debug for S3CredentialsDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3CredentialsDto")
            .field("access_key_id", &"***")
            .field("secret_access_key", &"***")
            .field("session_token", &self.session_token.as_ref().map(|_| "***"))
            .finish()
    }
}

/// S3 destination knobs the host grants through `DestinationContext.config`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OutputS3ContextDto {
    /// Scoped writable directory for this plugin only (`…/plugins/<id>/data`,
    /// wire `pluginDataDir`). Empty on the logical ABI (jail layout is
    /// transport-private).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub plugin_data_dir: String,
    /// Target bucket name.
    pub bucket: String,
    /// Key prefix under the bucket (`[output.s3].prefix`).
    pub prefix: String,
    /// AWS region (or compatible).
    pub region: String,
    /// Optional custom endpoint (MinIO / path-style hosts); host may prepend
    /// `https://` for bare hostnames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// When true, force path-style addressing (wire `forcePathStyle`).
    #[serde(default)]
    pub force_path_style: bool,
    /// When absent the guest may use the AWS SDK default provider chain
    /// (unconfined dev only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<S3CredentialsDto>,
}

/// Local filesystem destination knobs the host grants through
/// `DestinationContext.config`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OutputLocalContextDto {
    /// Scoped writable directory for this plugin only (`…/plugins/<id>/data`,
    /// wire `pluginDataDir`). Empty on the logical ABI (jail layout is
    /// transport-private).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub plugin_data_dir: String,
    /// Library output root (`[output.local].root`). Empty on the logical
    /// ABI; native guests read `BOOKCLERK_OUTPUT_LOCAL_ROOT` instead.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub root: String,
    /// Key prefix under `root` (`[output.local].prefix`).
    pub prefix: String,
}
