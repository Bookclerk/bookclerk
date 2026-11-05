//! Records every file key an acquire writes (`edition_files`).
//!
//! [`RecordingBackend`] wraps one destination's backend for one acquire
//! run and records each key the run writes against the title's edition,
//! just before the write, on that destination
//! ([`LibraryStore::record_edition_file`]).
//! The library's primary audio and PDF keys say nothing of a title's
//! chapter or part files and sidecars, so this record is what lets the
//! owner refuse a host upload another title's files in a folder titles
//! share, wherever the acquire wrote them.

use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use bookclerk_library::{FileOwner, FileRole, LibraryStore};
use bookclerk_storage::{
    ByteRange, ListPage, ObjectInfo, ObjectMeta, ObjectProbe, PutStreamResult, StorageBackend,
    StorageError,
};
use bytes::Bytes;
use tokio::io::AsyncRead;

use crate::destinations::{destination_id, AcquireDestination, AcquireDestinations};

/// One destination's backend whose writes are recorded for one edition.
pub(crate) struct RecordingBackend {
    /// The destination's own backend.
    inner: Box<dyn StorageBackend>,
    /// The library the record goes to.
    library: LibraryStore,
    /// The destination's id ([`destination_id`]).
    destination: &'static str,
    /// The edition the acquire writes.
    owner: FileOwner,
    /// The first audio key this run wrote here: its primary audio. Every
    /// later audio key (another part or chapter file) is a
    /// [`FileRole::Part`].
    primary: Arc<Mutex<Option<String>>>,
}

impl RecordingBackend {
    /// The role of `key` ([`FileRole::for_key`]): the first audio key
    /// written is the primary audio, and becomes the reference that makes
    /// every later audio key a part.
    fn role_of(&self, key: &str) -> FileRole {
        // A poisoned lock still holds the key it last stored.
        let mut primary = self
            .primary
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let role = FileRole::for_key(key, primary.as_deref());
        if role == FileRole::Audio && primary.is_none() {
            *primary = Some(key.to_owned());
        }
        role
    }

    /// Records `key` for the edition before it is written, with the role
    /// [`Self::role_of`] gives it.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Other`] when the record cannot be written;
    /// the write is then not attempted, so no file goes unrecorded.
    async fn record(&self, key: &str) -> bookclerk_storage::Result<()> {
        let role = self.role_of(key);
        self.library
            .record_edition_file(self.destination, key, &self.owner, role)
            .await
            .map_err(|err| {
                StorageError::Other(anyhow::anyhow!("could not record a title's file: {err}"))
            })
    }
}

/// `destinations` with each backend wrapped in a [`RecordingBackend`] for
/// the edition `owner`; the same backends unwrapped when the title has no
/// library row (`None`), since there is no edition to record them for.
pub(crate) fn recording_destinations(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    owner: Option<&FileOwner>,
) -> AcquireDestinations {
    let items = destinations
        .items
        .iter()
        .map(|item| AcquireDestination {
            kind: item.kind,
            backend: match owner {
                Some(owner) => Box::new(RecordingBackend {
                    inner: item.backend.clone_box(),
                    library: library.clone(),
                    destination: destination_id(item.kind),
                    owner: owner.clone(),
                    primary: Arc::default(),
                }),
                None => item.backend.clone_box(),
            },
            options: item.options.clone(),
        })
        .collect();
    AcquireDestinations {
        items,
        primary: destinations.primary,
        multi_destination: destinations.multi_destination,
    }
}

#[async_trait::async_trait]
impl StorageBackend for RecordingBackend {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn instance_id(&self) -> String {
        self.inner.instance_id()
    }

    fn scan_placement(&self) -> Option<String> {
        self.inner.scan_placement()
    }

    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(Self {
            inner: self.inner.clone_box(),
            library: self.library.clone(),
            destination: self.destination,
            owner: self.owner.clone(),
            primary: Arc::clone(&self.primary),
        })
    }

    async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> bookclerk_storage::Result<()> {
        self.record(key).await?;
        self.inner.put(key, data, meta).await
    }

    async fn put_file(
        &self,
        key: &str,
        path: &Path,
        meta: ObjectMeta,
    ) -> bookclerk_storage::Result<()> {
        self.record(key).await?;
        self.inner.put_file(key, path, meta).await
    }

    async fn get(&self, key: &str) -> bookclerk_storage::Result<Bytes> {
        self.inner.get(key).await
    }

    async fn exists(&self, key: &str) -> bookclerk_storage::Result<bool> {
        self.inner.exists(key).await
    }

    async fn list(&self, prefix: &str) -> bookclerk_storage::Result<Vec<ObjectInfo>> {
        self.inner.list(prefix).await
    }

    async fn list_audio(&self, prefix: &str) -> bookclerk_storage::Result<Vec<ObjectInfo>> {
        self.inner.list_audio(prefix).await
    }

    async fn probe(&self, key: &str) -> bookclerk_storage::Result<ObjectProbe> {
        self.inner.probe(key).await
    }

    async fn copy(&self, from: &str, to: &str) -> bookclerk_storage::Result<()> {
        self.record(to).await?;
        self.inner.copy(from, to).await
    }

    async fn rename(&self, from: &str, to: &str) -> bookclerk_storage::Result<()> {
        self.record(to).await?;
        self.inner.rename(from, to).await
    }

    async fn delete(&self, key: &str) -> bookclerk_storage::Result<()> {
        self.inner.delete(key).await
    }

    async fn head(&self, key: &str) -> bookclerk_storage::Result<Option<ObjectProbe>> {
        self.inner.head(key).await
    }

    async fn list_page(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> bookclerk_storage::Result<ListPage> {
        self.inner.list_page(prefix, cursor, limit).await
    }

    async fn get_stream(
        &self,
        key: &str,
        range: Option<ByteRange>,
    ) -> bookclerk_storage::Result<(ObjectProbe, Pin<Box<dyn AsyncRead + Send>>)> {
        self.inner.get_stream(key, range).await
    }

    async fn put_stream(
        &self,
        key: &str,
        body: Pin<Box<dyn AsyncRead + Send>>,
        meta: ObjectMeta,
    ) -> bookclerk_storage::Result<PutStreamResult> {
        self.record(key).await?;
        self.inner.put_stream(key, body, meta).await
    }

    fn supports_server_copy(&self) -> bool {
        self.inner.supports_server_copy()
    }

    async fn touch_file(
        &self,
        key: &str,
        created: Option<SystemTime>,
        modified: Option<SystemTime>,
    ) -> bookclerk_storage::Result<()> {
        self.inner.touch_file(key, created, modified).await
    }
}
