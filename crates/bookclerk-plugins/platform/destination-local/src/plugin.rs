//! Destination + job-handler surface for the local filesystem guest.

#![allow(clippy::missing_docs_in_private_items)]

use std::path::PathBuf;
use std::pin::Pin;

use async_trait::async_trait;
use bookclerk_plugin_sdk::{
    ByteRange, CopyResult, Destination, DestinationContext, JobHandler, ListOptions, ListPage,
    ObjectInfo, ObjectMetadata, PluginDescribe, PluginRoot, PutResult, ReadResult, ScalarLimits,
    Source, SourceContext, StreamCopyHandler, WorkerContext, WriteOptions, FEATURE_SCALAR_LIMITS,
    FEATURE_STORAGE_COPY, FEATURE_STREAMS, PRODUCT_API_VERSION,
};
use bookclerk_plugin_sdk::{OutputLocalContextDto, PluginError};
use bookclerk_storage::{LocalFsBackend, ObjectMeta, StorageBackend, StorageError};
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

/// Local filesystem destination capability.
pub struct LocalDestination {
    /// Filesystem backend rooted at the host-supplied output root.
    backend: LocalFsBackend,
}

impl LocalDestination {
    /// Builds a destination from [`DestinationContext`] JSON.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::invalid_params`] when the context JSON is not a
    /// local output context, or an internal error when the root cannot be opened.
    pub fn from_context(ctx: &DestinationContext) -> Result<Self> {
        let parsed: OutputLocalContextDto = if ctx.json.trim().is_empty() {
            OutputLocalContextDto {
                plugin_data_dir: String::new(),
                root: String::new(),
                prefix: String::new(),
            }
        } else {
            serde_json::from_str(&ctx.json).map_err(|err| {
                PluginError::invalid_params(format!("local destination context: {err}"))
            })?
        };
        let root = std::env::var_os("BOOKCLERK_OUTPUT_LOCAL_ROOT")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from(&parsed.root));
        if root.as_os_str().is_empty() {
            return Err(PluginError::invalid_params(
                "local destination root missing from transport env",
            ));
        }
        let backend = LocalFsBackend::with_prefix(root, &parsed.prefix).map_err(map_storage)?;
        Ok(Self { backend })
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
impl Destination for LocalDestination {
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
            key: dest_key,
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

#[async_trait(?Send)]
impl Source for LocalDestination {
    async fn open(&self, key: &str) -> Result<ReadResult> {
        Destination::get(self, key, None).await
    }
}

/// Root capability for the platform local destination guest.
pub struct LocalRoot;

#[async_trait(?Send)]
impl PluginRoot for LocalRoot {
    async fn describe(&self) -> Result<PluginDescribe> {
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: ID.into(),
            kind: "output".into(),
            display_name: Some("Local filesystem".into()),
            rpc_features: vec![
                FEATURE_SCALAR_LIMITS.into(),
                FEATURE_STREAMS.into(),
                FEATURE_STORAGE_COPY.into(),
            ],
            scalar_limits: ScalarLimits::default().into(),
            supported_roles: vec!["destination".into(), "source".into(), "worker".into()],
            ..PluginDescribe::default()
        })
    }

    async fn destination(&self, context: DestinationContext) -> Result<Box<dyn Destination>> {
        Ok(Box::new(LocalDestination::from_context(&context)?))
    }

    async fn source(&self, context: SourceContext) -> Result<Box<dyn Source>> {
        let dest_ctx = DestinationContext { json: context.json };
        Ok(Box::new(LocalDestination::from_context(&dest_ctx)?))
    }

    async fn worker(&self, _context: WorkerContext) -> Result<Box<dyn JobHandler>> {
        Ok(Box::new(StreamCopyHandler))
    }
}
