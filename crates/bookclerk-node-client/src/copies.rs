//! Downloading the files of a copy a library granted
//! (`bookclerk_node_proto::copies`): on a branch after
//! [`crate::HostClientHandle::copy_grant`], and on a clerk after its
//! socket's `copy.granted`.
//!
//! [`fetch_copy_file`] brings one file to a path on this install and
//! resumes a download that was cut short. The bytes go to disk as they
//! arrive; the library's answer decides nothing but whether they are the
//! bytes that were asked for
//! ([`crate::OwnerEndpoint::get_copy_stream`]).

use std::path::Path;

use bookclerk_node_proto::{is_copy_id, COPIES_PATH};

use crate::endpoint::{DialError, OwnerEndpoint};

/// Why [`fetch_copy_file`] did not bring the file.
#[derive(Debug, thiserror::Error)]
pub enum CopyFetchError {
    /// The copy id is not one a library gives out; nothing was asked.
    #[error("not a copy id")]
    InvalidCopy,
    /// The grant lists no file at that index; nothing was asked.
    #[error("the copy has no such file")]
    NoSuchFile,
    /// The library answered with this status instead of the file: `401`
    /// when the token ended, the branch is no longer trusted or the card
    /// is signed out (ask for a new grant), `404` when the copy has no
    /// such file.
    #[error("the library answered HTTP {0}")]
    Status(u16),
    /// The request failed, stalled, or the answer was not the bytes asked
    /// for. What arrived so far is kept, so the next call resumes.
    #[error(transparent)]
    Transfer(#[from] DialError),
    /// The local file could not be opened or read.
    #[error("the local file failed: {0}")]
    Local(#[from] std::io::Error),
}

/// Downloads file `index` of the copy `copy_id` to `dest`.
///
/// `dest` must be a path only this file of this copy is written to: what
/// it already holds is taken as the start of the file. A file already
/// `size` bytes long is complete and nothing is asked; a shorter one is
/// resumed with a `Range` request; a longer one is replaced. A library
/// that ignores the range sends the whole file, which replaces what was
/// there.
///
/// # Arguments
///
/// * `authorization` - The `Authorization` header value of the copy's
///   downloads ([`crate::GrantedCopy::authorization`] on a branch, the
///   copy scheme and the plain token on a clerk). Never logged.
/// * `size` - The file's exact size, as the grant's file list states it.
///
/// # Errors
///
/// Returns [`CopyFetchError`]; after `Transfer`, calling again resumes.
pub async fn fetch_copy_file(
    endpoint: &OwnerEndpoint,
    authorization: &str,
    copy_id: &str,
    index: usize,
    size: u64,
    dest: &Path,
) -> Result<(), CopyFetchError> {
    if !is_copy_id(copy_id) {
        return Err(CopyFetchError::InvalidCopy);
    }
    let path = format!("{COPIES_PATH}{copy_id}/files/{index}");
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dest)
        .await?;
    let held = file.metadata().await?.len();
    if held == size {
        return Ok(());
    }
    let mut resume_from = if held < size { held } else { 0 };
    loop {
        let status = endpoint
            .get_copy_stream(&path, authorization, resume_from, size, &mut file)
            .await?;
        match status {
            200 | 206 => {
                file.sync_all().await?;
                return Ok(());
            }
            // The library no longer takes the range (its file changed
            // under the same size): start over once.
            416 if resume_from > 0 => resume_from = 0,
            other => return Err(CopyFetchError::Status(other)),
        }
    }
}
