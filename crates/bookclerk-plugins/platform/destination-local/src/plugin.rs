//! Destination + job-handler surface for the local filesystem guest.

#![allow(clippy::missing_docs_in_private_items)]

use std::path::PathBuf;
use std::pin::Pin;

use async_trait::async_trait;
use bookclerk_plugin_sdk::manifest_capabilities;
use bookclerk_plugin_sdk::{
    Bindings, ByteRange, CopyResult, Destination, Entrypoints, ExtensibleConfig, Invocation,
    ListOptions, ListPage, ObjectInfo, ObjectMetadata, PluginDescribe, PluginWorker, PutResult,
    ReadResult, ScalarLimits, StreamCopyHandler, WriteOptions, FEATURE_SCALAR_LIMITS,
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
        StorageError::Integrity(msg) => PluginError::internal(format!("integrity: {msg}")),
        other => PluginError::internal(other.to_string()),
    }
}

/// Local filesystem destination capability.
pub struct LocalDestination {
    /// Filesystem backend rooted at the host-supplied output root.
    backend: LocalFsBackend,
}

impl LocalDestination {
    /// Builds a destination from the `open` [`Bindings::config`] payload
    /// (an [`OutputLocalContextDto`] as `application/json`; empty means defaults).
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::invalid_params`] when the payload is not a local
    /// output context, or an internal error when the root cannot be opened.
    pub fn from_config(config: &ExtensibleConfig) -> Result<Self> {
        let parsed: OutputLocalContextDto = if config.is_empty() {
            OutputLocalContextDto {
                plugin_data_dir: String::new(),
                root: String::new(),
                prefix: String::new(),
            }
        } else {
            config.json_into().map_err(|err| {
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
    let sha256 = probe.meta.sha256_hex.as_deref().and_then(|hex| {
        bookclerk_storage::parse_sha256_hex(hex)
            .ok()
            .map(|bytes| bytes.to_vec())
    });
    ObjectMetadata {
        key: probe.key,
        size: probe.size,
        content_type: probe.content_type.or(probe.meta.content_type),
        etag: probe.etag,
        sha256,
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
            .put_stream(&dest_key, body, write_meta(&options)?)
            .await
            .map_err(map_storage)?;
        Ok(PutResult {
            key: key.into(),
            bytes_written: written.bytes_written,
            etag: options.commit_token.clone().or(written.etag),
            sha256: put_sha(written.sha256_hex.as_deref()),
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

/// Root capability for the platform local destination guest.
pub struct LocalRoot;

#[async_trait(?Send)]
impl PluginWorker for LocalRoot {
    async fn describe(&self) -> Result<PluginDescribe> {
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: ID.into(),
            display_name: Some("Local filesystem".into()),
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
            storage: Some(Box::new(LocalDestination::from_config(&bindings.config)?)),
            job_runner: Some(Box::new(StreamCopyHandler)),
            ..Entrypoints::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn open_at(root: &std::path::Path) -> LocalDestination {
        std::env::set_var("BOOKCLERK_OUTPUT_LOCAL_ROOT", root);
        LocalDestination::from_config(&ExtensibleConfig::default()).expect("destination")
    }

    async fn stage(dest: &LocalDestination, key: &str, body: &[u8], token: &str) -> PutResult {
        dest.put(
            key,
            Box::pin(std::io::Cursor::new(body.to_vec())),
            WriteOptions {
                content_length: Some(body.len() as u64),
                commit_token: Some(token.into()),
                stage_only: true,
                ..WriteOptions::default()
            },
        )
        .await
        .expect("stage")
    }

    #[tokio::test]
    async fn commit_replay_after_rebuild_returns_digest_and_rejects_wrong_token() {
        let _guard = ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let key = "Author/book.m4b";
        let body = b"audiobook-bytes";
        let first = open_at(dir.path());
        stage(&first, key, body, "tok-1").await;
        let committed = first.commit(key, "tok-1").await.expect("commit");
        assert!(committed.sha256.is_some());
        assert_eq!(committed.bytes_written, body.len() as u64);
        drop(first);

        let second = open_at(dir.path());
        let replay = second.commit(key, "tok-1").await.expect("replay commit");
        assert_eq!(replay.sha256, committed.sha256);
        assert_eq!(replay.bytes_written, body.len() as u64);
        let wrong = second.commit(key, "tok-other").await.unwrap_err();
        let text = wrong.to_string();
        assert!(
            text.contains("not commit") || text.contains("not_found") || text.contains("NotFound"),
            "{text}"
        );
        std::env::remove_var("BOOKCLERK_OUTPUT_LOCAL_ROOT");
    }

    #[tokio::test]
    async fn same_stem_companions_keep_distinct_commit_tokens() {
        let _guard = ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let dest = open_at(dir.path());
        stage(&dest, "Title/book.m4b", b"audio", "audio-tok").await;
        stage(&dest, "Title/book.jpg", b"cover", "cover-tok").await;
        stage(&dest, "Title/book.pdf", b"pdf", "pdf-tok").await;
        let audio = dest.commit("Title/book.m4b", "audio-tok").await.unwrap();
        let cover = dest.commit("Title/book.jpg", "cover-tok").await.unwrap();
        let pdf = dest.commit("Title/book.pdf", "pdf-tok").await.unwrap();
        assert_ne!(audio.sha256, cover.sha256);
        assert_ne!(audio.sha256, pdf.sha256);
        drop(dest);

        let again = open_at(dir.path());
        assert!(again.commit("Title/book.m4b", "audio-tok").await.is_ok());
        assert!(again.commit("Title/book.jpg", "cover-tok").await.is_ok());
        assert!(again.commit("Title/book.pdf", "pdf-tok").await.is_ok());
        assert!(again.commit("Title/book.m4b", "cover-tok").await.is_err());
        assert!(again.commit("Title/book.jpg", "audio-tok").await.is_err());
        std::env::remove_var("BOOKCLERK_OUTPUT_LOCAL_ROOT");
    }
}
