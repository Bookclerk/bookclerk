//! Destination + job-handler surface for the S3 / MinIO guest.

#![allow(clippy::missing_docs_in_private_items)]

use async_trait::async_trait;
use bookclerk_plugin_sdk::manifest_capabilities;
use bookclerk_plugin_sdk::{
    Bindings, Entrypoints, ExtensibleConfig, Invocation, PluginDescribe, PluginWorker,
    ScalarLimits, StreamCopyHandler, FEATURE_SCALAR_LIMITS, FEATURE_STORAGE_COPY, FEATURE_STREAMS,
    PRODUCT_API_VERSION,
};
use bookclerk_plugin_sdk::{OutputS3ContextDto, PluginError, S3CredentialsDto};
use bookclerk_storage::guest;
use bookclerk_storage::s3_guest::ObjectStoreDestination;
use bookclerk_storage::{QueueUrl, S3Backend, S3Credentials, S3Settings, StorageCapabilities};

use crate::ID;

/// Result alias matching the ABI crate.
type Result<T> = std::result::Result<T, PluginError>;

/// Builds the destination from the `open` [`Bindings::config`] payload
/// (an [`OutputS3ContextDto`] as `application/json`).
///
/// # Errors
///
/// Returns invalid_params when the payload is not an S3 context, or internal
/// when the client cannot be constructed.
pub async fn destination_from_config(
    config: &ExtensibleConfig,
) -> Result<ObjectStoreDestination<S3Backend>> {
    let parsed: OutputS3ContextDto = config
        .json_into()
        .map_err(|err| PluginError::invalid_params(format!("s3 destination context: {err}")))?;
    Ok(ObjectStoreDestination::new(
        backend_from_ctx(&parsed).await?,
    ))
}

/// [`destination_from_config`] plus credentials from `SECRETS` when `CONFIG`
/// omits them.
///
/// The host keeps access keys out of `CONFIG` and the spawn config table.
///
/// # Errors
///
/// Returns invalid_params when the payload is not an S3 context, or internal
/// when the client cannot be constructed.
pub async fn destination_from_bindings(
    bindings: &Bindings,
) -> Result<ObjectStoreDestination<S3Backend>> {
    let mut parsed: OutputS3ContextDto = bindings
        .config
        .json_into()
        .map_err(|err| PluginError::invalid_params(format!("s3 destination context: {err}")))?;
    if parsed.credentials.is_none() {
        parsed.credentials = credentials_from_secrets(&bindings.secrets);
    }
    Ok(ObjectStoreDestination::new(
        backend_from_ctx(&parsed).await?,
    ))
}

/// Access keys from the `SECRETS` binding. `CONFIG` stays free of them.
fn credentials_from_secrets(secrets: &ExtensibleConfig) -> Option<S3CredentialsDto> {
    let value = secrets.json_value().ok()?;
    let access = secret_text(&value, &["accessKeyId", "access_key_id"]);
    let secret = secret_text(&value, &["secretAccessKey", "secret_access_key"]);
    if access.is_empty() || secret.is_empty() {
        return None;
    }
    let token = secret_text(&value, &["sessionToken", "session_token"]);
    Some(S3CredentialsDto {
        access_key_id: access,
        secret_access_key: secret,
        session_token: (!token.is_empty()).then_some(token),
    })
}

/// First non-empty string among `keys`.
fn secret_text(value: &serde_json::Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(text) = value.get(*key).and_then(serde_json::Value::as_str) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    String::new()
}

/// What the guest declares: every native operation of [`S3Backend`], and
/// `storage.changes`. The features are declared before the instance is
/// opened, so they cannot depend on its settings: an instance without a
/// notification queue answers `changes` with an incomplete page and the
/// host lists instead.
const CAPABILITIES: StorageCapabilities = StorageCapabilities {
    changes: true,
    ..S3Backend::CAPABILITIES
};

/// Builds an [`S3Backend`] from the host-injected bucket/region/endpoint context.
async fn backend_from_ctx(ctx: &OutputS3ContextDto) -> Result<S3Backend> {
    let cfg = S3Settings {
        bucket: ctx.bucket.clone(),
        region: ctx.region.clone(),
        endpoint: ctx.endpoint.clone(),
        force_path_style: ctx.force_path_style,
    };
    let creds = ctx.credentials.as_ref().map(credentials_from_dto);
    // The operator's setting is checked before anything about the guest's
    // own environment, so a bad URL is always reported as such.
    let queue = ctx
        .notification_queue_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(QueueUrl::parse)
        .transpose()
        .map_err(|err| PluginError::invalid_params(err.to_string()))?;
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| {
            PluginError::internal("s3 destination has no HOME for multipart recovery")
        })?;
    let journal = home.join("multipart-journal");
    let backend = S3Backend::from_parts_with_journal(&cfg, &ctx.prefix, creds.as_ref(), &journal)
        .await
        .map_err(|err| PluginError::internal(err.to_string()))?;
    match queue {
        Some(queue) => backend
            .with_notification_queue(queue, &home.join("notification-queue"))
            .map_err(|err| PluginError::internal(err.to_string())),
        None => Ok(backend),
    }
}

/// Copies ABI credential fields into the storage crate's credential struct.
fn credentials_from_dto(dto: &S3CredentialsDto) -> S3Credentials {
    S3Credentials {
        access_key_id: dto.access_key_id.clone(),
        secret_access_key: dto.secret_access_key.clone(),
        session_token: dto.session_token.clone(),
    }
}

/// Root capability for the optional S3 destination guest.
pub struct S3Root;

#[async_trait(?Send)]
impl PluginWorker for S3Root {
    async fn describe(&self) -> Result<PluginDescribe> {
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: ID.into(),
            display_name: Some("S3 / MinIO".into()),
            // The features are declared without the bucket's configuration,
            // so they cover every store: on one that does not enforce
            // conditional requests, the backend runs the host's algorithm
            // for each operation itself and reports `atomic: false`, and an
            // instance without a notification queue has no change feed.
            rpc_features: [
                FEATURE_SCALAR_LIMITS.into(),
                FEATURE_STREAMS.into(),
                FEATURE_STORAGE_COPY.into(),
            ]
            .into_iter()
            .chain(guest::storage_features(CAPABILITIES))
            .collect(),
            scalar_limits: ScalarLimits::default().into(),
            capabilities: manifest_capabilities(include_str!("../plugin.toml"))?,
            ..PluginDescribe::default()
        })
    }

    async fn open(&self, _invocation: Invocation, bindings: Bindings) -> Result<Entrypoints> {
        Ok(Entrypoints {
            storage: Some(Box::new(destination_from_bindings(&bindings).await?)),
            job_runner: Some(Box::new(StreamCopyHandler)),
            ..Entrypoints::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn describe_accepts_the_native_storage_operations() {
        let described = S3Root.describe().await.expect("describe");
        for feature in [
            bookclerk_plugin_sdk::FEATURE_STORAGE_MOVE,
            bookclerk_plugin_sdk::FEATURE_STORAGE_MOVE_MANY,
            bookclerk_plugin_sdk::FEATURE_STORAGE_DELETE_MANY,
            bookclerk_plugin_sdk::FEATURE_STORAGE_HEAD_MANY,
            bookclerk_plugin_sdk::FEATURE_STORAGE_CONDITIONAL_WRITE,
            bookclerk_plugin_sdk::FEATURE_STORAGE_CHANGES,
        ] {
            assert!(
                described.rpc_features.iter().any(|f| f == feature),
                "{feature} missing from {:?}",
                described.rpc_features
            );
        }
    }

    #[tokio::test]
    async fn a_queue_url_that_is_not_sqs_is_refused_at_open() {
        let config = ExtensibleConfig::json(&serde_json::json!({
            "bucket": "library",
            "prefix": "",
            "region": "us-east-1",
            "notificationQueueUrl": "https://example.com/123456789012/q",
        }));
        let err = destination_from_config(&config)
            .await
            .err()
            .expect("refused");
        assert!(err.to_string().contains("notification queue URL"), "{err}");
    }
}
