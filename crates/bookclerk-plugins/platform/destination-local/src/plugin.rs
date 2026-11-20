//! Destination + job-handler surface for the local filesystem guest.

#![allow(clippy::missing_docs_in_private_items)]

use std::path::PathBuf;
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
use bookclerk_plugin_sdk::{OutputLocalContextDto, PluginError};
use bookclerk_storage::guest::{self, object_metadata, plugin_error as map_storage};
use bookclerk_storage::{LocalFsBackend, ObjectMeta, StorageBackend, StorageError};
use tokio::io::AsyncRead;

use crate::ID;

/// Result alias matching the ABI crate.
type Result<T> = std::result::Result<T, PluginError>;

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
        let settings: WatchSetting = if config.is_empty() {
            WatchSetting::default()
        } else {
            config.json_into().map_err(|err| {
                PluginError::invalid_params(format!("local destination `watch` setting: {err}"))
            })?
        };
        let backend = if settings.watch {
            backend.with_watch()
        } else {
            backend
        };
        Ok(Self { backend })
    }
}

/// The instance's `watch` setting, read from the same `open` payload as the
/// root and prefix.
#[derive(Debug, Default, serde::Deserialize)]
struct WatchSetting {
    /// When true the guest watches the root and serves `changes` from what
    /// it saw (on a local disk only; see `docs/server/output.md`).
    #[serde(default)]
    watch: bool,
}

/// What this plugin does natively: everything the local backend does, plus
/// the change feed, which an instance turns on with its `watch` setting.
const CAPABILITIES: bookclerk_storage::StorageCapabilities =
    bookclerk_storage::StorageCapabilities {
        changes: true,
        ..LocalFsBackend::CAPABILITIES
    };

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

/// Root capability for the platform local destination guest.
pub struct LocalRoot;

#[async_trait(?Send)]
impl PluginWorker for LocalRoot {
    async fn describe(&self) -> Result<PluginDescribe> {
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: ID.into(),
            display_name: Some("Local filesystem".into()),
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

    #[tokio::test]
    async fn describe_accepts_the_native_storage_operations() {
        let described = LocalRoot.describe().await.expect("describe");
        for feature in [
            bookclerk_plugin_sdk::FEATURE_STORAGE_MOVE,
            bookclerk_plugin_sdk::FEATURE_STORAGE_MOVE_MANY,
            bookclerk_plugin_sdk::FEATURE_STORAGE_DELETE_MANY,
            bookclerk_plugin_sdk::FEATURE_STORAGE_HEAD_MANY,
            bookclerk_plugin_sdk::FEATURE_STORAGE_CONDITIONAL_WRITE,
        ] {
            assert!(
                described.rpc_features.iter().any(|f| f == feature),
                "{feature} missing from {:?}",
                described.rpc_features
            );
        }
        assert!(described
            .rpc_features
            .iter()
            .any(|f| f == bookclerk_plugin_sdk::FEATURE_STORAGE_CHANGES));
    }

    #[tokio::test]
    async fn the_watch_setting_turns_the_change_feed_on() {
        let _env = ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let off = open_at(dir.path());
        let page = off
            .changes(ChangesRequest::default())
            .await
            .expect("changes");
        assert!(!page.complete && !page.live_watch && !page.cursor.is_empty());

        let on = LocalDestination::from_config(&ExtensibleConfig::json(&serde_json::json!({
            "prefix": "",
            "watch": true,
        })))
        .expect("destination");
        std::env::remove_var("BOOKCLERK_OUTPUT_LOCAL_ROOT");
        let page = on
            .changes(ChangesRequest::default())
            .await
            .expect("changes");
        // A temp dir on a local disk is watched; elsewhere the feed says it
        // cannot vouch for itself.
        assert_eq!(page.complete, page.live_watch);
        let invalid = on
            .changes(ChangesRequest {
                cursor: Some("not a cursor".into()),
                limit: 0,
            })
            .await
            .expect_err("a foreign cursor");
        assert_eq!(
            invalid.code,
            bookclerk_plugin_sdk::PluginErrorCode::InvalidCursor
        );
    }

    #[tokio::test]
    async fn conditional_puts_and_moves_answer_conflict_through_the_destination() {
        let _guard = ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let dest = open_at(dir.path());
        let put_if_absent = |body: &'static [u8]| WriteOptions {
            content_length: Some(body.len() as u64),
            if_none_match: true,
            ..WriteOptions::default()
        };
        let first = dest
            .put(
                "a.json",
                Box::pin(std::io::Cursor::new(b"one".to_vec())),
                put_if_absent(b"one"),
            )
            .await
            .expect("first write");
        let probe = dest.head("a.json").await.unwrap().expect("probe");
        assert_eq!(first.etag, probe.etag);
        let again = dest
            .put(
                "a.json",
                Box::pin(std::io::Cursor::new(b"two".to_vec())),
                put_if_absent(b"two"),
            )
            .await
            .unwrap_err();
        assert_eq!(again.code, bookclerk_plugin_sdk::PluginErrorCode::Conflict);
        let staged = dest
            .put(
                "b.json",
                Box::pin(std::io::Cursor::new(b"x".to_vec())),
                WriteOptions {
                    stage_only: true,
                    commit_token: Some("t".into()),
                    ..put_if_absent(b"x")
                },
            )
            .await
            .unwrap_err();
        assert_eq!(
            staged.code,
            bookclerk_plugin_sdk::PluginErrorCode::InvalidParams
        );

        let moved = dest
            .move_object("a.json", "c/a.json", MoveOptions::default())
            .await
            .expect("move");
        assert!(moved.atomic);
        let read = dest.head("c/a.json").await.unwrap().expect("probe").etag;
        dest.put(
            "c/a.json",
            Box::pin(std::io::Cursor::new(b"rewritten".to_vec())),
            WriteOptions::default(),
        )
        .await
        .expect("rewrite");
        let stale = dest
            .move_object(
                "c/a.json",
                "d.json",
                MoveOptions {
                    if_source_match: read,
                    ..MoveOptions::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(stale.code, bookclerk_plugin_sdk::PluginErrorCode::Conflict);
        std::env::remove_var("BOOKCLERK_OUTPUT_LOCAL_ROOT");
    }
}
