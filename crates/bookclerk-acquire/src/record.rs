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

use bookclerk_library::{FileOwner, FileProbe, FileRole, LibraryStore};
use bookclerk_storage::{
    ByteRange, ListPage, ObjectInfo, ObjectMeta, ObjectProbe, PutStreamResult, StorageBackend,
    StorageError,
};
use bytes::Bytes;
use tokio::io::AsyncRead;

use bookclerk_config::{DestinationId, OutputCopy};

use crate::destinations::{AcquireDestination, AcquireDestinations};
use crate::library_copies::StoredFrom;

/// One destination's backend whose writes are recorded for one edition.
pub(crate) struct RecordingBackend {
    /// The destination's own backend.
    inner: Box<dyn StorageBackend>,
    /// The library the record goes to.
    library: LibraryStore,
    /// The destination's id.
    destination: DestinationId,
    /// The edition the acquire writes.
    owner: FileOwner,
    /// Plain id of the storefront the acquire downloads from, when it is
    /// one a file row may name ([`source_store_of`]).
    source_store: Option<String>,
    /// The storefront the stored audio came from instead, once a
    /// library's copy from another store replaced the acquire's own
    /// download.
    stored_from: StoredFrom,
    /// The pool whose confined worker probes written audio.
    media: Arc<bookclerk_media::MediaPool>,
    /// The first audio key this run wrote here: its primary audio. Every
    /// later audio key (another part or chapter file) is a
    /// [`FileRole::Part`].
    primary: Arc<Mutex<Option<String>>>,
    /// The library copy (`output.copies`) this run writes here, named on
    /// the row of each audio file; `None` names no copy.
    copy: Option<OutputCopy>,
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

    /// Probes the audio file just written at `key` from `path` and stores
    /// the result on its row, with the storefront the copy came from
    /// ([`LibraryStore::set_edition_file_probe`]).
    ///
    /// Only audio is probed (the primary file, parts and chapters). A file
    /// that cannot be probed (a format the tag reader does not know, or no
    /// confined worker where one is required) keeps a row with no probe:
    /// the file is stored either way, and the acquire does not fail for it.
    async fn probe(&self, key: &str, path: &Path, role: FileRole) {
        match role {
            FileRole::Audio | FileRole::Part | FileRole::Chapter => {}
            FileRole::Pdf
            | FileRole::Cover
            | FileRole::Sidecar
            | FileRole::Transcript
            | FileRole::Epub => return,
        }
        let probe = match self.media.probe_file(path).await {
            Ok(probe) => FileProbe {
                content_type: probe.content_type.to_owned(),
                bitrate_kbps: probe
                    .bitrate_kbps
                    .map(|kbps| kbps.min(FileProbe::MAX_BITRATE_KBPS)),
                lossless: probe.lossless,
                source_store: self
                    .stored_from
                    .get()
                    .and_then(|store| source_store_of(&store))
                    .or_else(|| self.source_store.clone()),
            },
            Err(err) => {
                tracing::debug!(key, error = %err, "written audio file was not probed");
                return;
            }
        };
        if let Err(err) = self
            .library
            .set_edition_file_probe(self.destination.as_str(), key, &self.owner, &probe)
            .await
        {
            tracing::warn!(key, error = %err, "could not store a file's probe result");
        }
    }

    /// Records `key` for the edition before it is written, with the role
    /// [`Self::role_of`] gives it.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Other`] when the record cannot be written,
    /// or when another live edition holds `key`; the write is then not
    /// attempted, so no file goes unrecorded and no edition's file is
    /// replaced by another's.
    async fn record(&self, key: &str) -> bookclerk_storage::Result<FileRole> {
        let role = self.role_of(key);
        let copy = match role {
            FileRole::Audio | FileRole::Part | FileRole::Chapter => self.copy.as_ref(),
            FileRole::Pdf
            | FileRole::Cover
            | FileRole::Sidecar
            | FileRole::Transcript
            | FileRole::Epub => None,
        };
        let recorded = self
            .library
            .record_edition_copy_file(self.destination.as_str(), key, &self.owner, role, copy)
            .await
            .map_err(|err| {
                StorageError::Other(anyhow::anyhow!("could not record a title's file: {err}"))
            })?;
        if recorded {
            Ok(role)
        } else {
            Err(StorageError::Other(anyhow::anyhow!(
                "{key} is another edition's file; refusing to replace it"
            )))
        }
    }
}

/// `source` as a file row may name it: a store's plain id (lowercase
/// letters, digits, `_`, `-` and `.`, at most
/// [`FileProbe::MAX_CONTENT_TYPE_BYTES`] bytes). Anything else is left out,
/// so an odd plugin id costs the row its source and not the probe.
fn source_store_of(source: &str) -> Option<String> {
    let plain = !source.is_empty()
        && source.len() <= FileProbe::MAX_CONTENT_TYPE_BYTES
        && source
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.'));
    plain.then(|| source.to_owned())
}

/// `destinations` with each backend wrapped in a [`RecordingBackend`] for
/// the edition `owner`; the same backends unwrapped when the title has no
/// library row (`None`), since there is no edition to record them for.
pub(crate) fn recording_destinations(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    owner: Option<&FileOwner>,
    source: &str,
    stored_from: &StoredFrom,
) -> AcquireDestinations {
    recording_destinations_probed_by(
        library,
        destinations,
        owner,
        source,
        stored_from,
        bookclerk_media::pool(),
    )
}

/// [`recording_destinations`] whose audio rows name the library copy
/// `copy` the run writes, and whose first audio key on each destination is
/// that copy's primary audio.
pub(crate) fn recording_copy_destinations(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    owner: Option<&FileOwner>,
    source: &str,
    stored_from: &StoredFrom,
    copy: &OutputCopy,
) -> AcquireDestinations {
    recording_with(
        library,
        destinations,
        owner,
        source,
        stored_from,
        bookclerk_media::pool(),
        Some(copy),
    )
}

/// [`recording_destinations`] whose written audio is probed through
/// `media` instead of the process-wide pool.
pub(crate) fn recording_destinations_probed_by(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    owner: Option<&FileOwner>,
    source: &str,
    stored_from: &StoredFrom,
    media: Arc<bookclerk_media::MediaPool>,
) -> AcquireDestinations {
    recording_with(
        library,
        destinations,
        owner,
        source,
        stored_from,
        media,
        None,
    )
}

/// The recording wrap of [`recording_destinations_probed_by`], naming
/// `copy` on audio rows.
fn recording_with(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    owner: Option<&FileOwner>,
    source: &str,
    stored_from: &StoredFrom,
    media: Arc<bookclerk_media::MediaPool>,
    copy: Option<&OutputCopy>,
) -> AcquireDestinations {
    let items = destinations
        .items
        .iter()
        .map(|item| AcquireDestination {
            id: item.id.clone(),
            backend: match owner {
                Some(owner) => Box::new(RecordingBackend {
                    inner: item.backend.clone_box(),
                    library: library.clone(),
                    destination: item.id.clone(),
                    owner: owner.clone(),
                    source_store: source_store_of(source),
                    stored_from: stored_from.clone(),
                    media: Arc::clone(&media),
                    primary: Arc::default(),
                    copy: copy.cloned(),
                }),
                None => item.backend.clone_box(),
            },
            options: item.options.clone(),
            filter: item.filter.clone(),
        })
        .collect();
    AcquireDestinations {
        items,
        primary: destinations.primary.clone(),
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
            destination: self.destination.clone(),
            owner: self.owner.clone(),
            source_store: self.source_store.clone(),
            stored_from: self.stored_from.clone(),
            media: Arc::clone(&self.media),
            primary: Arc::clone(&self.primary),
            copy: self.copy.clone(),
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
        let role = self.record(key).await?;
        self.inner.put_file(key, path, meta).await?;
        self.probe(key, path, role).await;
        Ok(())
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
