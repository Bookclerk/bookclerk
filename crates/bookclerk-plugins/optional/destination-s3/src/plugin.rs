//! Destination + job-handler surface for the S3 / MinIO guest.

#![allow(clippy::missing_docs_in_private_items)]

use std::pin::Pin;

use async_trait::async_trait;
use bookclerk_plugin_sdk::manifest_capabilities;
use bookclerk_plugin_sdk::{
    Bindings, ByteRange, ChangesPage, ChangesRequest, CopyResult, Destination, Entrypoints,
    ExtensibleConfig, Invocation, KeyOutcome, ListOptions, ListPage, MoveManyPage, MoveManyRequest,
    MoveOptions, MoveResult, ObjectMetadata, PluginDescribe, PluginWorker, PutResult, ReadResult,
    ScalarLimits, StreamCopyHandler, WriteOptions, FEATURE_SCALAR_LIMITS, FEATURE_STORAGE_COPY,
    FEATURE_STREAMS, PRODUCT_API_VERSION,
};
use bookclerk_plugin_sdk::{OutputS3ContextDto, PluginError, S3CredentialsDto};
use bookclerk_storage::guest::{self, object_metadata, plugin_error as map_storage};
use bookclerk_storage::{
    ObjectMeta, QueueUrl, S3Backend, S3Credentials, S3Settings, StorageBackend,
    StorageCapabilities, StorageError,
};
use tokio::io::AsyncRead;

use crate::ID;

/// Result alias matching the ABI crate.
type Result<T> = std::result::Result<T, PluginError>;

/// S3 destination capability (multipart sink for streamed puts).
pub struct S3Destination {
    /// In-guest S3 backend.
    backend: S3Backend,
}

impl S3Destination {
    /// Builds a destination from the `open` [`Bindings::config`] payload
    /// (an [`OutputS3ContextDto`] as `application/json`).
    ///
    /// # Errors
    ///
    /// Returns invalid_params when the payload is not an S3 context, or internal
    /// when the client cannot be constructed.
    pub async fn from_config(config: &ExtensibleConfig) -> Result<Self> {
        let parsed: OutputS3ContextDto = config
            .json_into()
            .map_err(|err| PluginError::invalid_params(format!("s3 destination context: {err}")))?;
        let backend = backend_from_ctx(&parsed).await?;
        Ok(Self { backend })
    }

    /// [`Self::from_config`] plus credentials from `SECRETS` when `CONFIG` omits them.
    ///
    /// The host keeps access keys out of `CONFIG` and the spawn config table.
    ///
    /// # Errors
    ///
    /// Returns invalid_params when the payload is not an S3 context, or internal
    /// when the client cannot be constructed.
    pub async fn from_bindings(bindings: &Bindings) -> Result<Self> {
        let mut parsed: OutputS3ContextDto = bindings
            .config
            .json_into()
            .map_err(|err| PluginError::invalid_params(format!("s3 destination context: {err}")))?;
        if parsed.credentials.is_none() {
            parsed.credentials = credentials_from_secrets(&bindings.secrets);
        }
        let backend = backend_from_ctx(&parsed).await?;
        Ok(Self { backend })
    }
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

fn write_meta(options: &WriteOptions) -> Result<ObjectMeta> {
    let sha256_hex =
        bookclerk_storage::sha256_field_from_raw(options.sha256.as_deref()).map_err(map_storage)?;
    Ok(ObjectMeta {
        content_type: options.content_type.clone(),
        content_length: options.content_length,
        sha256_hex,
        commit_token: options.commit_token.clone(),
        ..Default::default()
    })
}

fn put_sha(hex: Option<&str>) -> Option<Vec<u8>> {
    hex.and_then(|hex| {
        bookclerk_storage::parse_sha256_hex(hex)
            .ok()
            .map(|bytes| bytes.to_vec())
    })
}

/// Destination-side staging key. Bytes never spool on the host or broker.
fn stage_object_key(token: Option<&str>, key: &str) -> Result<String> {
    let token = token.unwrap_or("");
    if token.is_empty()
        || token.contains('/')
        || token.contains('\\')
        || token.contains("..")
        || token.contains('\0')
    {
        return Err(PluginError::invalid_params(
            "commit_token must be a non-empty identifier without path separators",
        ));
    }
    if key.is_empty() {
        return Err(PluginError::invalid_params("object key required"));
    }
    Ok(format!(".bookclerk-stage/{token}/{key}"))
}

#[async_trait(?Send)]
impl Destination for S3Destination {
    async fn head(&self, key: &str) -> Result<Option<ObjectMetadata>> {
        self.backend
            .head(key)
            .await
            .map(|probe| probe.map(object_metadata))
            .map_err(map_storage)
    }

    async fn list(&self, options: ListOptions) -> Result<ListPage> {
        guest::list(&self.backend, options).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<ReadResult> {
        let storage_range = range.map(|r| bookclerk_storage::ByteRange {
            offset: r.offset,
            length: r.length,
        });
        let (probe, body) = self
            .backend
            .get_stream(key, storage_range)
            .await
            .map_err(map_storage)?;
        Ok(ReadResult {
            meta: object_metadata(probe),
            body,
        })
    }

    async fn put(
        &self,
        key: &str,
        body: Pin<Box<dyn AsyncRead + Send>>,
        options: WriteOptions,
    ) -> Result<PutResult> {
        let condition = guest::write_condition(&options)?;
        let dest_key = if options.stage_only {
            stage_object_key(options.commit_token.as_deref(), key)?
        } else {
            key.to_string()
        };
        let meta = write_meta(&options)?;
        let written = match &condition {
            Some(condition) => {
                self.backend
                    .put_stream_conditional(&dest_key, body, meta, condition)
                    .await
                    .map_err(map_storage)?
                    .put
            }
            None => self
                .backend
                .put_stream(&dest_key, body, meta)
                .await
                .map_err(map_storage)?,
        };
        Ok(PutResult {
            key: key.into(),
            bytes_written: written.bytes_written,
            etag: options.commit_token.clone().or(written.etag),
            sha256: put_sha(written.sha256_hex.as_deref()),
            ..Default::default()
        })
    }

    async fn copy(&self, from: &str, to: &str) -> Result<CopyResult> {
        let probe = self.backend.probe(from).await.map_err(map_storage)?;
        self.backend.copy(from, to).await.map_err(map_storage)?;
        Ok(CopyResult {
            bytes_copied: probe.size,
        })
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.backend.delete(key).await.map_err(map_storage)
    }

    async fn commit(&self, key: &str, commit_token: &str) -> Result<PutResult> {
        let staged = stage_object_key(Some(commit_token), key)?;
        match self.backend.probe(&staged).await {
            Ok(probe) => {
                self.backend.copy(&staged, key).await.map_err(map_storage)?;
                let _ = self.backend.delete(&staged).await;
                Ok(PutResult {
                    key: key.into(),
                    bytes_written: probe.size,
                    etag: Some(commit_token.into()),
                    sha256: put_sha(probe.meta.sha256_hex.as_deref()),
                    ..Default::default()
                })
            }
            Err(StorageError::NotFound(_)) => {
                let probe = self.backend.probe(key).await.map_err(map_storage)?;
                if probe.meta.commit_token.as_deref() != Some(commit_token) {
                    return Err(PluginError::not_found(format!(
                        "staged object missing and `{key}` is not commit {commit_token}"
                    )));
                }
                Ok(PutResult {
                    key: key.into(),
                    bytes_written: probe.size,
                    etag: Some(commit_token.into()),
                    sha256: put_sha(probe.meta.sha256_hex.as_deref()),
                    ..Default::default()
                })
            }
            Err(err) => Err(map_storage(err)),
        }
    }

    async fn move_object(&self, from: &str, to: &str, options: MoveOptions) -> Result<MoveResult> {
        guest::move_object(&self.backend, from, to, options).await
    }

    async fn move_many(&self, request: MoveManyRequest) -> Result<MoveManyPage> {
        guest::move_many(&self.backend, request).await
    }

    async fn delete_many(&self, keys: Vec<String>) -> Result<Vec<KeyOutcome>> {
        guest::delete_many(&self.backend, keys).await
    }

    async fn head_many(&self, keys: Vec<String>) -> Result<Vec<Option<ObjectMetadata>>> {
        guest::head_many(&self.backend, keys).await
    }

    async fn translate_keys(&self, keys: Vec<String>) -> Result<Vec<String>> {
        guest::translate_keys(&self.backend, keys).await
    }

    async fn changes(&self, request: ChangesRequest) -> Result<ChangesPage> {
        guest::changes(&self.backend, request).await
    }

    async fn abort_stage(&self, key: &str, commit_token: &str) -> Result<()> {
        let staged = stage_object_key(Some(commit_token), key)?;
        match self.backend.delete(&staged).await {
            Ok(()) => Ok(()),
            Err(StorageError::NotFound(_)) => Ok(()),
            Err(err) => Err(map_storage(err)),
        }
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
            storage: Some(Box::new(S3Destination::from_bindings(&bindings).await?)),
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
        let err = S3Destination::from_config(&config)
            .await
            .err()
            .expect("refused");
        assert!(err.to_string().contains("notification queue URL"), "{err}");
    }
}
