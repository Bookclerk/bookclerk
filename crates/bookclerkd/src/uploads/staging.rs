//! Where an ingest batch's bytes wait on this node, and how they get there.
//!
//! Each batch has one folder, `$BOOKCLERK_FILES_DIR/uploads/<batch id>/`:
//! `files/<member number>` holds each member's bytes (named by number, so
//! an uploaded name never reaches the filesystem) and `extract/<member
//! number>/` is the empty folder an archive member expands into before its
//! entries move to `files/`. Nothing else writes under `uploads/`, so a
//! folder whose batch is gone is an orphan the prune loop removes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};

use axum::body::Body;
use bookclerk_library::control_plane::IngestBatchId;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

/// The folder that holds every batch's staging folder.
///
/// # Arguments
///
/// * `files_dir` - `$BOOKCLERK_FILES_DIR`.
pub(crate) fn root(files_dir: &Path) -> PathBuf {
    files_dir.join("uploads")
}

/// The staging folder of batch `id`.
pub(crate) fn batch_dir(files_dir: &Path, id: &IngestBatchId) -> PathBuf {
    root(files_dir).join(id.as_str())
}

/// The staged bytes of member `member_no` of batch `id`.
pub(crate) fn member_file(files_dir: &Path, id: &IngestBatchId, member_no: u32) -> PathBuf {
    batch_dir(files_dir, id)
        .join("files")
        .join(member_no.to_string())
}

/// The folder archive member `member_no` of batch `id` expands into.
pub(crate) fn extract_dir(files_dir: &Path, id: &IngestBatchId, member_no: u32) -> PathBuf {
    batch_dir(files_dir, id)
        .join("extract")
        .join(member_no.to_string())
}

/// Removes batch `id`'s staging folder; a missing folder is not an error.
///
/// # Errors
///
/// Returns the I/O error of a folder that exists but could not be removed.
pub(crate) async fn remove_batch(files_dir: &Path, id: &IngestBatchId) -> std::io::Result<()> {
    match tokio::fs::remove_dir_all(batch_dir(files_dir, id)).await {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

/// The names of the folders under [`root`] that are batch ids; anything
/// else there is left alone.
///
/// # Errors
///
/// Returns the I/O error of a root that exists but could not be listed.
pub(crate) async fn staged_batches(files_dir: &Path) -> std::io::Result<Vec<IngestBatchId>> {
    let mut entries = match tokio::fs::read_dir(root(files_dir)).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut out = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if let Some(id) = entry.file_name().to_str().and_then(IngestBatchId::parse) {
            out.push(id);
        }
    }
    Ok(out)
}

/// True when writing `extra` more bytes would leave the disk holding
/// `files_dir` with less than `floor_percent` of its size free.
///
/// A disk whose space cannot be read counts as having room: the write
/// itself still fails when the disk is full.
///
/// # Arguments
///
/// * `files_dir` - `$BOOKCLERK_FILES_DIR`.
/// * `extra` - Bytes about to be written.
/// * `floor_percent` - The `uploads.free_disk_floor_percent` setting.
pub(crate) fn below_floor(files_dir: &Path, extra: u64, floor_percent: u8) -> bool {
    let probe = if files_dir.exists() {
        files_dir
    } else {
        files_dir.parent().unwrap_or(files_dir)
    };
    let Some(space) = bookclerk_sandbox::filesystem_space(probe) else {
        return false;
    };
    let floor = space.total / 100 * u64::from(floor_percent);
    space.available.saturating_sub(extra) < floor
}

/// Members whose bytes a request is writing now, so two requests never
/// append to one file at once.
static WRITING: LazyLock<Mutex<HashSet<(String, u32)>>> = LazyLock::new(Default::default);

/// Holds a member's write slot until dropped.
pub(crate) struct WriteSlot {
    /// The member's key in [`WRITING`].
    key: (String, u32),
}

impl Drop for WriteSlot {
    fn drop(&mut self) {
        WRITING
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key);
    }
}

/// Takes member `member_no` of batch `id`'s write slot, or `None` while
/// another request holds it.
pub(crate) fn try_write_slot(id: &IngestBatchId, member_no: u32) -> Option<WriteSlot> {
    let key = (id.as_str().to_string(), member_no);
    let fresh = WRITING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key.clone());
    // Built only when taken: dropping a slot frees its key, so a slot made
    // for a held key would free the other request's.
    if fresh {
        Some(WriteSlot { key })
    } else {
        None
    }
}

/// Why an append wrote nothing.
#[derive(Debug)]
pub(crate) enum AppendError {
    /// The staged file holds a different number of bytes than the client
    /// resumed from.
    OffsetMismatch {
        /// Bytes the staged file holds.
        current: u64,
    },
    /// The body runs past the member's declared size; nothing from it was
    /// kept.
    TooLong,
    /// The staged file could not be opened, written or synced.
    Io(std::io::Error),
}

/// What an append kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Appended {
    /// Bytes the staged file holds now.
    pub len: u64,
    /// True when the body ended early (the client went away); the bytes
    /// that arrived are kept so the client can resume after them.
    pub interrupted: bool,
}

/// Appends `body` to the staged file at `path`, which must hold exactly
/// `offset` bytes, without passing `size` bytes in all.
///
/// Bytes are synced to disk before this returns, so a recorded offset is
/// never ahead of the file.
///
/// # Arguments
///
/// * `path` - The member's staged file; created (with its folder) if
///   missing.
/// * `offset` - Where the client resumes, from its `Upload-Offset` header.
/// * `size` - The member's declared size.
/// * `body` - The request body.
///
/// # Errors
///
/// [`AppendError`] naming why nothing was kept.
pub(crate) async fn append(
    path: &Path,
    offset: u64,
    size: u64,
    body: Body,
) -> Result<Appended, AppendError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(AppendError::Io)?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .await
        .map_err(AppendError::Io)?;
    let current = file.metadata().await.map_err(AppendError::Io)?.len();
    if current != offset {
        return Err(AppendError::OffsetMismatch { current });
    }
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(AppendError::Io)?;
    let mut len = offset;
    let mut interrupted = false;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            interrupted = true;
            break;
        };
        let chunk_len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
        if len.saturating_add(chunk_len) > size {
            file.set_len(offset).await.map_err(AppendError::Io)?;
            file.sync_all().await.map_err(AppendError::Io)?;
            return Err(AppendError::TooLong);
        }
        file.write_all(&chunk).await.map_err(AppendError::Io)?;
        len += chunk_len;
    }
    file.flush().await.map_err(AppendError::Io)?;
    file.sync_all().await.map_err(AppendError::Io)?;
    Ok(Appended { len, interrupted })
}

/// The lowercase hex SHA-256 of the file at `path`, read off the async
/// runtime.
///
/// # Errors
///
/// Returns the I/O error of a file that could not be read.
pub(crate) async fn sha256_file(path: &Path) -> std::io::Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            let read = file.read(&mut buf)?;
            if read == 0 {
                break;
            }
            hasher.update(buf.get(..read).unwrap_or_default());
        }
        Ok(hex::encode(hasher.finalize()))
    })
    .await
    .map_err(std::io::Error::other)?
}

/// The first `n` bytes of the file at `path` (fewer for a shorter file).
///
/// # Errors
///
/// Returns the I/O error of a file that could not be read.
pub(crate) async fn head(path: &Path, n: usize) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(path).await?;
    let mut out = Vec::with_capacity(n);
    file.take(u64::try_from(n).unwrap_or(u64::MAX))
        .read_to_end(&mut out)
        .await?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn appends_resume_at_the_staged_length_and_refuse_overruns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("files").join("0");
        let first = append(&path, 0, 6, Body::from("abc")).await.unwrap();
        assert_eq!(
            first,
            Appended {
                len: 3,
                interrupted: false
            }
        );
        let err = append(&path, 0, 6, Body::from("abc")).await.unwrap_err();
        assert!(matches!(err, AppendError::OffsetMismatch { current: 3 }));
        let err = append(&path, 3, 6, Body::from("defg")).await.unwrap_err();
        assert!(matches!(err, AppendError::TooLong));
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        let done = append(&path, 3, 6, Body::from("def")).await.unwrap();
        assert_eq!(done.len, 6);
        assert_eq!(
            sha256_file(&path).await.unwrap(),
            hex::encode(Sha256::digest(b"abcdef"))
        );
        assert_eq!(head(&path, 2).await.unwrap(), b"ab");
    }

    #[test]
    fn one_request_writes_a_member_at_a_time() {
        let id = IngestBatchId::new();
        let slot = try_write_slot(&id, 1).unwrap();
        assert!(try_write_slot(&id, 1).is_none());
        assert!(try_write_slot(&id, 2).is_some());
        drop(slot);
        assert!(try_write_slot(&id, 1).is_some());
    }

    #[tokio::test]
    async fn only_batch_folders_count_as_staged() {
        let dir = tempfile::tempdir().unwrap();
        assert!(staged_batches(dir.path()).await.unwrap().is_empty());
        let id = IngestBatchId::new();
        std::fs::create_dir_all(batch_dir(dir.path(), &id)).unwrap();
        std::fs::create_dir_all(root(dir.path()).join("not-a-batch")).unwrap();
        assert_eq!(staged_batches(dir.path()).await.unwrap(), vec![id.clone()]);
        remove_batch(dir.path(), &id).await.unwrap();
        remove_batch(dir.path(), &id).await.unwrap();
        assert!(staged_batches(dir.path()).await.unwrap().is_empty());
    }

    #[test]
    fn a_zero_floor_never_blocks() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!below_floor(dir.path(), 0, 0));
        assert!(
            below_floor(dir.path(), u64::MAX, 1)
                || bookclerk_sandbox::filesystem_space(dir.path()).is_none()
        );
    }
}
