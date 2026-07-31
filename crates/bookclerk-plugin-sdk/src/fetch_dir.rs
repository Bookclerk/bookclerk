//! Receive host-passed descriptors for fetch and upload side channels.

#![cfg_attr(unix, allow(unsafe_code))]

use std::ops::Deref;
use std::path::{Path, PathBuf};

use crate::error::{Result, SdkError};
use crate::pass_fd::{fd_proc_path, recv_passed_fd, PLUGIN_FD_CHANNEL_ENV};
use crate::protocol::FetchTitleParams;

/// An open fetch work directory received from the host.
///
/// Holds the descriptor open for its lifetime so `/proc/self/fd/N` (or
/// `/dev/fd/N`) stays valid for the duration of the `fetch_title` call.
pub struct FetchWorkDir {
    #[cfg(unix)]
    _fd: Option<std::os::fd::OwnedFd>,
    path: PathBuf,
}

impl FetchWorkDir {
    /// Resolve the directory a `fetch_title` call should write into.
    pub fn open(params: &FetchTitleParams) -> Result<Self> {
        if std::env::var(PLUGIN_FD_CHANNEL_ENV).is_ok() {
            let fd = recv_passed_fd()?;
            return owned_fd_path(fd).map(|(owned, path)| Self {
                #[cfg(unix)]
                _fd: Some(owned),
                path,
            });
        }
        Ok(Self {
            #[cfg(unix)]
            _fd: None,
            path: PathBuf::from(&params.cache_dir),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Deref for FetchWorkDir {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

/// An open upload file received from the host for `put_file`.
pub struct UploadFile {
    #[cfg(unix)]
    _fd: Option<std::os::fd::OwnedFd>,
    path: PathBuf,
}

impl UploadFile {
    /// Resolve the local file path for a `put_file` call.
    pub fn open(local_path: Option<&str>) -> Result<Self> {
        if std::env::var(PLUGIN_FD_CHANNEL_ENV).is_ok() {
            let fd = recv_passed_fd()?;
            return owned_fd_path(fd).map(|(owned, path)| Self {
                #[cfg(unix)]
                _fd: Some(owned),
                path,
            });
        }
        let path = local_path
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                SdkError::message(
                    "put_file requires a side-channel descriptor or an explicit local_path",
                )
            })?;
        Ok(Self {
            #[cfg(unix)]
            _fd: None,
            path,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Deref for UploadFile {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

/// Resolve the directory a `fetch_title` call should write into.
pub fn fetch_work_dir(params: &FetchTitleParams) -> Result<FetchWorkDir> {
    FetchWorkDir::open(params)
}

/// Resolve the local file path for a `put_file` call.
pub fn upload_file_path(local_path: Option<&str>) -> Result<UploadFile> {
    UploadFile::open(local_path)
}

#[cfg(unix)]
fn owned_fd_path(fd: i32) -> Result<(std::os::fd::OwnedFd, PathBuf)> {
    use std::os::fd::{AsRawFd, FromRawFd};

    // SAFETY: fd was received from the host for this RPC and is not used elsewhere.
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    let path = fd_proc_path(owned.as_raw_fd());
    Ok((owned, path))
}

#[cfg(not(unix))]
fn owned_fd_path(_fd: i32) -> Result<((), PathBuf)> {
    Err(SdkError::message(
        "descriptor side channel is not supported on this platform".into(),
    ))
}
