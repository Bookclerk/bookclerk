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
//!
//! Every backend of one acquire run shares an [`AcquireRun`]: the run's own
//! writer id ([`AcquireWriter`]), so a write that fails releases exactly the
//! record this run made for it, and the keys each of the run's copies
//! (`output.copies`) wrote, so no copy writes at a key another copy of the
//! same run wrote, on any destination. Nor does a copy write where an
//! earlier run recorded a file of another copy the library makes.

use std::collections::BTreeMap;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use bookclerk_library::{
    AcquireWriter, AudioStream, FileOwner, FileProbe, FileRole, LibraryStore, RecordedFile,
};
use bookclerk_storage::{
    ByteRange, ListPage, ObjectInfo, ObjectMeta, ObjectProbe, PutStreamResult, StorageBackend,
    StorageError,
};
use bytes::Bytes;
use tokio::io::AsyncRead;

use bookclerk_config::{DestinationId, OutputCopy};

use crate::destinations::{AcquireDestination, AcquireDestinations};
use crate::library_copies::StoredFrom;

/// What every recording backend of one acquire run shares.
#[derive(Debug)]
pub(crate) struct AcquireRun {
    /// The writer id the run records its keys under.
    writer: AcquireWriter,
    /// Each key a copy of this run wrote (or is writing), with that copy.
    /// Keys are compared across destinations: the library finds a copy's
    /// files by key, so two copies at one key, even on two destinations,
    /// would let it take one for the other.
    copy_keys: Mutex<BTreeMap<String, OutputCopy>>,
    /// The library copies whose files, recorded by earlier runs, no copy
    /// of this run may replace: every copy the library makes.
    guarded: Vec<OutputCopy>,
}

impl AcquireRun {
    /// A new run with a writer id of its own, guarding no earlier copy's
    /// files.
    pub(crate) fn new() -> Arc<Self> {
        Self::guarding(Vec::new())
    }

    /// A new run with a writer id of its own whose copies never write
    /// where an earlier run recorded a file of another of the `guarded`
    /// copies ([`LibraryStore::key_held_by_another_copy`]).
    pub(crate) fn guarding(guarded: Vec<OutputCopy>) -> Arc<Self> {
        Arc::new(Self {
            writer: AcquireWriter::for_run(),
            copy_keys: Mutex::default(),
            guarded,
        })
    }

    /// Claims `key` for `copy` within this run.
    ///
    /// # Returns
    ///
    /// The other copy of this run that already wrote `key`, when there is
    /// one; `key` is then left to it.
    fn claim_for_copy(&self, key: &str, copy: &OutputCopy) -> Option<OutputCopy> {
        // A poisoned lock still holds the keys it last stored.
        let mut keys = self
            .copy_keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match keys.get(key) {
            Some(other) if other != copy => Some(other.clone()),
            Some(_) => None,
            None => {
                keys.insert(key.to_owned(), copy.clone());
                None
            }
        }
    }
}

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
    /// What the run's backends share.
    run: Arc<AcquireRun>,
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
        let probed = match self.media.probe_file(path).await {
            Ok(probed) => probed,
            Err(err) => {
                tracing::debug!(key, error = %err, "written audio file was not probed");
                return;
            }
        };
        let probe = FileProbe {
            content_type: probed.content_type.to_owned(),
            bitrate_kbps: probed
                .bitrate_kbps
                .map(|kbps| kbps.min(FileProbe::MAX_BITRATE_KBPS)),
            lossless: probed.lossless,
            source_store: self
                .stored_from
                .get()
                .and_then(|store| source_store_of(&store))
                .or_else(|| self.source_store.clone()),
        };
        let destination = self.destination.as_str();
        match self
            .library
            .set_edition_file_probe(destination, key, &self.owner, &probe)
            .await
        {
            Ok(true) => {}
            // Another edition took the key over: nothing to describe.
            Ok(false) => return,
            Err(err) => {
                tracing::warn!(key, error = %err, "could not store a file's probe result");
                return;
            }
        }
        let stream = AudioStream {
            codec: probed.codec,
            format: probed.format,
            duration_ms: probed.duration_ms,
        };
        if let Err(err) = self
            .library
            .set_edition_file_stream(destination, key, &self.owner, &stream)
            .await
        {
            tracing::warn!(key, error = %err, "could not store a file's audio stream");
        }
    }

    /// Records `key` for the edition before it is written, with the role
    /// [`Self::role_of`] gives it, under the run's writer.
    ///
    /// # Returns
    ///
    /// The role, and whether the record is new
    /// ([`RecordedFile::New`]: [`Self::release`] it when the write fails).
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Other`] when the record cannot be written,
    /// when another live edition holds `key`, or when another copy of this
    /// run already wrote `key` ([`AcquireRun::claim_for_copy`]) or an
    /// earlier run recorded it for another copy the library makes; the write
    /// is then not attempted, so no file goes unrecorded and no edition's
    /// or copy's file is replaced by another's.
    async fn record(&self, key: &str) -> bookclerk_storage::Result<(FileRole, RecordedFile)> {
        if let Some(copy) = &self.copy {
            if let Some(other) = self.run.claim_for_copy(key, copy) {
                return Err(copy_collision(key, &other, copy));
            }
            let earlier = self
                .library
                .key_held_by_another_copy(&self.owner, key, copy, &self.run.guarded)
                .await
                .map_err(|err| {
                    StorageError::Other(anyhow::anyhow!("could not read a title's files: {err}"))
                })?;
            if let Some(other) = earlier {
                return Err(copy_collision(key, &other, copy));
            }
        }
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
            .record_run_edition_file(
                self.destination.as_str(),
                key,
                &self.owner,
                role,
                copy,
                &self.run.writer,
            )
            .await
            .map_err(|err| {
                StorageError::Other(anyhow::anyhow!("could not record a title's file: {err}"))
            })?;
        match recorded {
            RecordedFile::New | RecordedFile::TakenOver => Ok((role, recorded)),
            RecordedFile::Refused => Err(StorageError::Other(anyhow::anyhow!(
                "{key} is another edition's file; refusing to replace it"
            ))),
        }
    }

    /// After a write of `key` failed: releases the record [`Self::record`]
    /// made when it was new, so a later host upload of the same title may
    /// claim the key. A record that was already the edition's stays, since
    /// its earlier file may still be there. Best effort: a release that
    /// fails is logged, and the write's own error is what the caller sees.
    async fn release(&self, key: &str, recorded: RecordedFile) {
        match recorded {
            RecordedFile::New => {}
            RecordedFile::TakenOver | RecordedFile::Refused => return,
        }
        if let Err(err) = self
            .library
            .release_run_edition_file(self.destination.as_str(), key, &self.run.writer)
            .await
        {
            tracing::warn!(key, error = %err, "could not release a failed write's file record");
        }
    }

    /// Records `key`, runs `write`, and releases a new record when the
    /// write fails ([`Self::release`]).
    ///
    /// # Returns
    ///
    /// The role the key was recorded with.
    ///
    /// # Errors
    ///
    /// Returns the record's refusal or the write's error.
    async fn recorded_write<F>(&self, key: &str, write: F) -> bookclerk_storage::Result<FileRole>
    where
        F: std::future::Future<Output = bookclerk_storage::Result<()>>,
    {
        let (role, recorded) = self.record(key).await?;
        if let Err(err) = write.await {
            self.release(key, recorded).await;
            return Err(err);
        }
        Ok(role)
    }
}

/// The refusal of a write of `key` by `copy` where the `other` copy's file
/// is.
fn copy_collision(key: &str, other: &OutputCopy, copy: &OutputCopy) -> StorageError {
    StorageError::Other(anyhow::anyhow!(
        "{key} is the {other} copy's file; the {copy} copy would replace it \
         (give the destinations templates or path limits that keep each \
         copy's folder apart)"
    ))
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
    run: &Arc<AcquireRun>,
) -> AcquireDestinations {
    recording_destinations_probed_by(
        library,
        destinations,
        owner,
        source,
        stored_from,
        bookclerk_media::pool(),
        run,
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
    run: &Arc<AcquireRun>,
) -> AcquireDestinations {
    recording_with(
        library,
        destinations,
        owner,
        source,
        stored_from,
        bookclerk_media::pool(),
        Some(copy),
        run,
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
    run: &Arc<AcquireRun>,
) -> AcquireDestinations {
    recording_with(
        library,
        destinations,
        owner,
        source,
        stored_from,
        media,
        None,
        run,
    )
}

/// The recording wrap of [`recording_destinations_probed_by`], naming
/// `copy` on audio rows, for the run `run`.
#[allow(clippy::too_many_arguments)]
fn recording_with(
    library: &LibraryStore,
    destinations: &AcquireDestinations,
    owner: Option<&FileOwner>,
    source: &str,
    stored_from: &StoredFrom,
    media: Arc<bookclerk_media::MediaPool>,
    copy: Option<&OutputCopy>,
    run: &Arc<AcquireRun>,
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
                    run: Arc::clone(run),
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
            run: Arc::clone(&self.run),
        })
    }

    async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> bookclerk_storage::Result<()> {
        self.recorded_write(key, self.inner.put(key, data, meta))
            .await
            .map(|_| ())
    }

    async fn put_file(
        &self,
        key: &str,
        path: &Path,
        meta: ObjectMeta,
    ) -> bookclerk_storage::Result<()> {
        let role = self
            .recorded_write(key, self.inner.put_file(key, path, meta))
            .await?;
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
        self.recorded_write(to, self.inner.copy(from, to))
            .await
            .map(|_| ())
    }

    async fn rename(&self, from: &str, to: &str) -> bookclerk_storage::Result<()> {
        self.recorded_write(to, self.inner.rename(from, to))
            .await
            .map(|_| ())
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
        let (_, recorded) = self.record(key).await?;
        match self.inner.put_stream(key, body, meta).await {
            Ok(put) => Ok(put),
            Err(err) => {
                self.release(key, recorded).await;
                Err(err)
            }
        }
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
