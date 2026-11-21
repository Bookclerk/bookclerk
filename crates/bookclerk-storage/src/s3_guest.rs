//! A storage guest's `Destination` capability over an [`S3Backend`].
//!
//! The `s3`, `r2` and `b2` destination plugins all store through an
//! [`S3Backend`] (each built its own way: endpoint, credentials, change
//! feed), and answer the plugin ABI the same way: streamed writes with
//! optional conditions, staged writes committed by token, and the intent
//! operations through [`crate::guest`].

use std::pin::Pin;

use async_trait::async_trait;
use bookclerk_plugin_sdk::{
    ByteRange, ChangesPage, ChangesRequest, CopyResult, Destination, KeyOutcome, ListOptions,
    ListPage, MoveManyPage, MoveManyRequest, MoveOptions, MoveResult, ObjectMetadata, PluginError,
    PutResult, ReadResult, WriteOptions,
};
use tokio::io::AsyncRead;

use crate::guest::{self, object_metadata, plugin_error as map_storage};
use crate::{ObjectMeta, S3Backend, StorageBackend, StorageError};

/// Result alias matching the ABI crate.
type Result<T> = std::result::Result<T, PluginError>;

/// The `Destination` capability of an object-store guest.
pub struct ObjectStoreDestination {
    /// The backend every call goes to.
    backend: S3Backend,
}

impl ObjectStoreDestination {
    /// Serves `backend` as a destination.
    #[must_use]
    pub fn new(backend: S3Backend) -> Self {
        Self { backend }
    }
}

/// The object metadata a write asks for.
fn write_meta(options: &WriteOptions) -> Result<ObjectMeta> {
    let sha256_hex =
        crate::sha256_field_from_raw(options.sha256.as_deref()).map_err(map_storage)?;
    Ok(ObjectMeta {
        content_type: options.content_type.clone(),
        content_length: options.content_length,
        sha256_hex,
        commit_token: options.commit_token.clone(),
        ..Default::default()
    })
}

/// The SHA-256 a write reports, as raw bytes.
fn put_sha(hex: Option<&str>) -> Option<Vec<u8>> {
    hex.and_then(|hex| {
        crate::parse_sha256_hex(hex)
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
impl Destination for ObjectStoreDestination {
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
        let storage_range = range.map(|r| crate::ByteRange {
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
