//! Destination + job-handler surface for the S3 / MinIO guest.

#![allow(clippy::missing_docs_in_private_items)]

use std::pin::Pin;

use async_trait::async_trait;
use bookclerk_config::OutputS3Config;
use bookclerk_plugin_sdk::manifest_capabilities;
use bookclerk_plugin_sdk::{
    Bindings, ByteRange, CopyResult, Destination, Entrypoints, ExtensibleConfig, Invocation,
    ListOptions, ListPage, ObjectInfo, ObjectMetadata, PluginDescribe, PluginWorker, PutResult,
    ReadResult, ScalarLimits, StreamCopyHandler, WriteOptions, FEATURE_SCALAR_LIMITS,
    FEATURE_STORAGE_COPY, FEATURE_STREAMS, PRODUCT_API_VERSION,
};
use bookclerk_plugin_sdk::{OutputS3ContextDto, PluginError, S3CredentialsDto};
use bookclerk_storage::{ObjectMeta, S3Backend, S3Credentials, StorageBackend, StorageError};
use tokio::io::AsyncRead;

use crate::ID;

/// Result alias matching the ABI crate.
type Result<T> = std::result::Result<T, PluginError>;

/// Maps storage errors onto ABI plugin errors.
fn map_storage(err: StorageError) -> PluginError {
    match err {
        StorageError::NotFound(key) => PluginError::not_found(key),
        StorageError::PayloadTooLarge(msg) => PluginError::payload_too_large(msg),
        StorageError::InvalidCursor(msg) => PluginError::invalid_cursor(msg),
        other => PluginError::internal(other.to_string()),
    }
}

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
}

/// Builds an [`S3Backend`] from the host-injected bucket/region/endpoint context.
async fn backend_from_ctx(ctx: &OutputS3ContextDto) -> Result<S3Backend> {
    let cfg = OutputS3Config {
        enabled: true,
        bucket: ctx.bucket.clone(),
        prefix: ctx.prefix.clone(),
        region: ctx.region.clone(),
        endpoint: ctx.endpoint.clone(),
        force_path_style: ctx.force_path_style,
        naming: Default::default(),
    };
    let creds = ctx.credentials.as_ref().map(credentials_from_dto);
    S3Backend::from_parts(&cfg, &ctx.prefix, creds.as_ref())
        .await
        .map_err(|err| PluginError::internal(err.to_string()))
}

/// Copies ABI credential fields into the storage crate's credential struct.
fn credentials_from_dto(dto: &S3CredentialsDto) -> S3Credentials {
    S3Credentials {
        access_key_id: dto.access_key_id.clone(),
        secret_access_key: dto.secret_access_key.clone(),
        session_token: dto.session_token.clone(),
        label: None,
    }
}

fn meta_from_probe(probe: bookclerk_storage::ObjectProbe) -> ObjectMetadata {
    ObjectMetadata {
        key: probe.key,
        size: probe.size,
        content_type: probe.content_type.or(probe.meta.content_type),
        etag: None,
        sha256: None,
    }
}

fn write_meta(options: &WriteOptions) -> ObjectMeta {
    ObjectMeta {
        content_type: options.content_type.clone(),
        content_length: options.content_length,
        ..Default::default()
    }
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
            .map(|probe| probe.map(meta_from_probe))
            .map_err(map_storage)
    }

    async fn list(&self, options: ListOptions) -> Result<ListPage> {
        let page = self
            .backend
            .list_page(&options.prefix, options.cursor.as_deref(), options.limit)
            .await
            .map_err(map_storage)?;
        Ok(ListPage {
            objects: page
                .objects
                .into_iter()
                .map(|obj| ObjectInfo {
                    key: obj.key,
                    size: obj.size,
                })
                .collect(),
            next_cursor: page.next_cursor,
        })
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
            meta: meta_from_probe(probe),
            body,
        })
    }

    async fn put(
        &self,
        key: &str,
        body: Pin<Box<dyn AsyncRead + Send>>,
        options: WriteOptions,
    ) -> Result<PutResult> {
        let dest_key = if options.stage_only {
            stage_object_key(options.commit_token.as_deref(), key)?
        } else {
            key.to_string()
        };
        let written = self
            .backend
            .put_stream(&dest_key, body, write_meta(&options))
            .await
            .map_err(map_storage)?;
        Ok(PutResult {
            key: key.into(),
            bytes_written: written.bytes_written,
            etag: options.commit_token.clone().or(written.etag),
            sha256: None,
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
                    sha256: None,
                })
            }
            Err(StorageError::NotFound(_)) => {
                let probe = self.backend.probe(key).await.map_err(map_storage)?;
                Ok(PutResult {
                    key: key.into(),
                    bytes_written: probe.size,
                    etag: Some(commit_token.into()),
                    sha256: None,
                })
            }
            Err(err) => Err(map_storage(err)),
        }
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
            rpc_features: vec![
                FEATURE_SCALAR_LIMITS.into(),
                FEATURE_STREAMS.into(),
                FEATURE_STORAGE_COPY.into(),
            ],
            scalar_limits: ScalarLimits::default().into(),
            capabilities: manifest_capabilities(include_str!("../plugin.toml"))?,
            ..PluginDescribe::default()
        })
    }

    async fn open(&self, _invocation: Invocation, bindings: Bindings) -> Result<Entrypoints> {
        Ok(Entrypoints {
            storage: Some(Box::new(
                S3Destination::from_config(&bindings.config).await?,
            )),
            job_runner: Some(Box::new(StreamCopyHandler)),
            ..Entrypoints::default()
        })
    }
}
